//! Centralized debug logging for psmux.
//!
//! All logs write to `~/.psmux/` and are gated by environment variables.
//! Nothing is stored in the repo or source tree — only in the user's
//! home directory under `.psmux/`.
//!
//! ## Environment Variables
//!
//! | Variable               | Log file                          | Description                          |
//! |------------------------|-----------------------------------|--------------------------------------|
//! | `PSMUX_CLIENT_DEBUG=1` | `~/.psmux/client_debug.log`       | Client TUI rendering, draw, status   |
//! | `PSMUX_STYLE_DEBUG=1`  | `~/.psmux/style_debug.log`        | Style/theme parsing, inline styles   |/// | `PSMUX_INPUT_DEBUG=1`  | `~/.psmux/input_debug.log`        | Every crossterm event + console mode |//! | `PSMUX_MOUSE_DEBUG=1`  | `~/.psmux/mouse_debug.log`        | Mouse injection (existing)           |
//! | `PSMUX_SSH_DEBUG=1`    | `~/.psmux/ssh_input.log`          | SSH input handling (existing)        |
//! | `PSMUX_LATENCY_LOG=1`  | `~/.psmux/latency.log`            | Keypress-to-render latency (existing)|
//!
//! All loggers are:
//! - **Off by default** — zero overhead when disabled (one atomic load per call)
//! - **Capped** — auto-stop after N entries to prevent disk fill
//! - **Thread-safe** — use `LazyLock<Mutex<Option<File>>>`
//! - **Timestamped** — `[HH:MM:SS.mmm]` prefix on every line
//! - **Truncated on startup** — fresh log each session (no stale data)

use std::io::Write;
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};

/// Resolve the psmux data directory (`~/.psmux/`).
fn psmux_dir() -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    format!("{}/.psmux", home)
}

/// Open a log file in the psmux data directory, creating the directory if needed.
/// Returns `None` if the file cannot be created.
fn open_log(filename: &str) -> Option<std::fs::File> {
    let dir = psmux_dir();
    let _ = std::fs::create_dir_all(&dir);
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true) // fresh log each session
        .write(true)
        .open(format!("{}/{}", dir, filename))
        .ok()
}

/// Check if an env var is set to a truthy value ("1" or "true").
fn env_enabled(var: &str) -> bool {
    std::env::var(var).map_or(false, |v| v == "1" || v.eq_ignore_ascii_case("true"))
}

// ─── Client debug log ───────────────────────────────────────────────────────

/// Client debug log file, gated by `PSMUX_CLIENT_DEBUG=1`.
/// Covers: frame receive, JSON parse, draw lifecycle, status bar rendering.
static CLIENT_LOG: LazyLock<Mutex<Option<std::fs::File>>> = LazyLock::new(|| {
    if !env_enabled("PSMUX_CLIENT_DEBUG") { return Mutex::new(None); }
    Mutex::new(open_log("client_debug.log"))
});

static CLIENT_LOG_COUNT: AtomicU32 = AtomicU32::new(0);

/// Maximum log entries per session to prevent disk fill.
const CLIENT_LOG_CAP: u32 = 5000;

/// Log a client debug message. No-op unless `PSMUX_CLIENT_DEBUG=1`.
///
/// # Arguments
/// * `component` — short tag like `"frame"`, `"draw"`, `"status"`, `"parse"`
/// * `msg` — the log message (should not contain newlines)
pub fn client_log(component: &str, msg: &str) {
    let n = CLIENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= CLIENT_LOG_CAP {
        if n == CLIENT_LOG_CAP {
            // Log one final "cap reached" message
            if let Ok(mut guard) = CLIENT_LOG.lock() {
                if let Some(ref mut f) = *guard {
                    let _ = writeln!(f, "[{}][log] --- log cap reached ({} entries), further logging suppressed ---",
                        chrono::Local::now().format("%H:%M:%S%.3f"), CLIENT_LOG_CAP);
                    let _ = f.flush();
                }
            }
        }
        return;
    }
    if let Ok(mut guard) = CLIENT_LOG.lock() {
        if let Some(ref mut f) = *guard {
            let _ = writeln!(f, "[{}][{}] {}",
                chrono::Local::now().format("%H:%M:%S%.3f"), component, msg);
            let _ = f.flush();
        }
    }
}

/// Returns `true` if client debug logging is active.
pub fn client_log_enabled() -> bool {
    CLIENT_LOG.lock().ok().map_or(false, |g| g.is_some())
}

// ─── Style debug log ────────────────────────────────────────────────────────

/// Style/theme parsing debug log, gated by `PSMUX_STYLE_DEBUG=1`.
/// Covers: inline style parsing, unclosed directives, color mapping.
static STYLE_LOG: LazyLock<Mutex<Option<std::fs::File>>> = LazyLock::new(|| {
    if !env_enabled("PSMUX_STYLE_DEBUG") { return Mutex::new(None); }
    Mutex::new(open_log("style_debug.log"))
});

static STYLE_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
const STYLE_LOG_CAP: u32 = 2000;

/// Log a style debug message. No-op unless `PSMUX_STYLE_DEBUG=1`.
pub fn style_log(component: &str, msg: &str) {
    let n = STYLE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= STYLE_LOG_CAP {
        if n == STYLE_LOG_CAP {
            if let Ok(mut guard) = STYLE_LOG.lock() {
                if let Some(ref mut f) = *guard {
                    let _ = writeln!(f, "[{}][log] --- log cap reached ---",
                        chrono::Local::now().format("%H:%M:%S%.3f"));
                    let _ = f.flush();
                }
            }
        }
        return;
    }
    if let Ok(mut guard) = STYLE_LOG.lock() {
        if let Some(ref mut f) = *guard {
            let _ = writeln!(f, "[{}][{}] {}",
                chrono::Local::now().format("%H:%M:%S%.3f"), component, msg);
            let _ = f.flush();
        }
    }
}

/// Returns `true` if style debug logging is active.
pub fn style_log_enabled() -> bool {
    STYLE_LOG.lock().ok().map_or(false, |g| g.is_some())
}

// ─── Input debug log ────────────────────────────────────────────────────────

/// Input event debug log, gated by `PSMUX_INPUT_DEBUG=1`.
/// Traces every crossterm event + console input mode at startup.
static INPUT_LOG: LazyLock<Mutex<Option<std::fs::File>>> = LazyLock::new(|| {
    if !env_enabled("PSMUX_INPUT_DEBUG") { return Mutex::new(None); }
    Mutex::new(open_log("input_debug.log"))
});

static INPUT_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
const INPUT_LOG_CAP: u32 = 10000;

/// Log an input debug message. No-op unless `PSMUX_INPUT_DEBUG=1`.
pub fn input_log(component: &str, msg: &str) {
    let n = INPUT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= INPUT_LOG_CAP {
        if n == INPUT_LOG_CAP {
            if let Ok(mut guard) = INPUT_LOG.lock() {
                if let Some(ref mut f) = *guard {
                    let _ = writeln!(f, "[{}][log] --- log cap reached ---",
                        chrono::Local::now().format("%H:%M:%S%.3f"));
                    let _ = f.flush();
                }
            }
        }
        return;
    }
    if let Ok(mut guard) = INPUT_LOG.lock() {
        if let Some(ref mut f) = *guard {
            let _ = writeln!(f, "[{}][{}] {}",
                chrono::Local::now().format("%H:%M:%S%.3f"), component, msg);
            let _ = f.flush();
        }
    }
}

/// Returns `true` if input debug logging is active.
pub fn input_log_enabled() -> bool {
    INPUT_LOG.lock().ok().map_or(false, |g| g.is_some())
}

// ─── Server debug log ───────────────────────────────────────────────────────

/// Server debug log, gated by `PSMUX_SERVER_DEBUG=1`.
/// Traces active_idx changes, command dispatch, etc.
static SERVER_LOG: LazyLock<Mutex<Option<std::fs::File>>> = LazyLock::new(|| {
    if !env_enabled("PSMUX_SERVER_DEBUG") { return Mutex::new(None); }
    Mutex::new(open_log("server_debug.log"))
});

static SERVER_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
const SERVER_LOG_CAP: u32 = 10000;

/// Log a server debug message. No-op unless `PSMUX_SERVER_DEBUG=1`.
pub fn server_log(component: &str, msg: &str) {
    let n = SERVER_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= SERVER_LOG_CAP {
        if n == SERVER_LOG_CAP {
            if let Ok(mut guard) = SERVER_LOG.lock() {
                if let Some(ref mut f) = *guard {
                    let _ = writeln!(f, "[{}][log] --- log cap reached ---",
                        chrono::Local::now().format("%H:%M:%S%.3f"));
                    let _ = f.flush();
                }
            }
        }
        return;
    }
    if let Ok(mut guard) = SERVER_LOG.lock() {
        if let Some(ref mut f) = *guard {
            let _ = writeln!(f, "[{}][{}] {}",
                chrono::Local::now().format("%H:%M:%S%.3f"), component, msg);
            let _ = f.flush();
        }
    }
}

/// Returns `true` if server debug logging is active.
pub fn server_log_enabled() -> bool {
    SERVER_LOG.lock().ok().map_or(false, |g| g.is_some())
}

// ─── Spawn diagnostics log (ALWAYS ON) ──────────────────────────────────────
//
// Unlike the gated logs above, spawn diagnostics are always enabled: pane
// spawns are rare (user-initiated), so the cost is negligible, and the
// "new window instantly dies with `The handle is invalid`" failure mode
// (2026-06-26 / 2026-07-13, Ops session) is state-dependent and impossible
// to diagnose after the fact without a persistent record.  The log answers:
//   - which spawns happened, in which session, at what ConPTY size
//   - whether the child died young (startup crash) and with what exit code
//   - what the server's own console state looked like at that moment
//   - how recently a console inject cycle (AttachConsole/FreeConsole) ran
//
// File: `~/.psmux/spawn_diag.log`, shared by all servers (each line carries
// the writer's pid + session), append-mode, truncated at open when >2 MB.

static SPAWN_LOG: LazyLock<Mutex<Option<std::fs::File>>> = LazyLock::new(|| {
    let dir = psmux_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = format!("{}/spawn_diag.log", dir);
    // Trim the shared file when it grows past 2 MB.  Losing old history at
    // the boundary is acceptable; unbounded growth is not.
    let too_big = std::fs::metadata(&path).map_or(false, |m| m.len() > 2 * 1024 * 1024);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(!too_big)
        .truncate(too_big)
        .write(true)
        .open(&path)
        .ok();
    Mutex::new(file)
});

static SPAWN_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
const SPAWN_LOG_CAP: u32 = 20_000;

/// Session name for spawn log lines, set once at server startup.
static SPAWN_LOG_SESSION: Mutex<String> = Mutex::new(String::new());

/// Record the session name this process serves; prefixes every spawn log line.
pub fn set_spawn_log_session(name: &str) {
    if let Ok(mut s) = SPAWN_LOG_SESSION.lock() {
        *s = name.to_string();
    }
}

/// Log a spawn-diagnostics message. Always on.
///
/// `component` — short tag like `"spawn"`, `"early-death"`, `"warm"`, `"inject"`.
pub fn spawn_log(component: &str, msg: &str) {
    let n = SPAWN_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= SPAWN_LOG_CAP { return; }
    let session = SPAWN_LOG_SESSION.lock().map(|s| s.clone()).unwrap_or_default();
    if let Ok(mut guard) = SPAWN_LOG.lock() {
        if let Some(ref mut f) = *guard {
            let _ = writeln!(f, "[{}][pid={} s={}][{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                std::process::id(), session, component, msg);
            let _ = f.flush();
        }
    }
}

// ─── Pane spawn-time registry ───────────────────────────────────────────────
//
// Maps pane_id → (spawn Instant, child pid).  Lets `prune_exited` (which sees
// a child exit but knows nothing about when it started) distinguish a normal
// exit from a startup crash and log the latter loudly with the child's age.

static SPAWN_TIMES: LazyLock<Mutex<std::collections::HashMap<usize, (std::time::Instant, u32)>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Register a freshly spawned pane child. Call right after spawn_command().
pub fn record_pane_spawn(pane_id: usize, child_pid: u32) {
    if let Ok(mut m) = SPAWN_TIMES.lock() {
        m.insert(pane_id, (std::time::Instant::now(), child_pid));
    }
}

/// Report a pane child exit. Returns `(age_ms, child_pid)` if the spawn was
/// registered. Removes the entry.
pub fn record_pane_exit(pane_id: usize) -> Option<(u128, u32)> {
    SPAWN_TIMES.lock().ok()?.remove(&pane_id)
        .map(|(t, pid)| (t.elapsed().as_millis(), pid))
}

/// Threshold below which a child exit after spawn is logged as a startup
/// crash ("early death") with full console-state diagnostics.
pub const EARLY_DEATH_MS: u128 = 5_000;
