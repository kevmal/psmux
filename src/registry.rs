//! Session registry: the `~/.psmux/<base>.port` / `.key` / `.pid` files and
//! the rules for creating, checking, and removing them safely.
//!
//! A session's identity IS its registration files, so the failure mode this
//! module exists to prevent is name theft: registrations used to be managed
//! with bare `read`/`write`/`remove_file` calls and a single 100ms TCP
//! connect as the liveness test.  On a busy or freshly-woken machine that
//! check fails for perfectly healthy servers, and callers reacted by
//! deleting the port file and claiming the name for a new server — leaving
//! the old server running but unreachable (a "zombie" whose attached
//! clients still work, but which `psmux ls`/`-t name` can no longer see).
//! Worse, the zombie still believes it owns the name, so when it later
//! exits it deletes the *new* session's registration, perpetuating the
//! cycle.
//!
//! Rules enforced here:
//!   1. A registration records the owning PID (`<base>.pid`) next to the
//!      port and key files.
//!   2. Liveness = TCP probe answered with a psmux AUTH banner (retried
//!      with rising timeouts), OR — probe failing — the registered PID
//!      still running a psmux-family image.  Only when both fail is a
//!      registration stale.
//!   3. Registration files are only deleted by their owner
//!      (compare-before-delete on the port number) or after a full
//!      staleness verdict.
//!   4. Fresh servers claim a name atomically (`create_new`) so two
//!      concurrent spawns cannot both believe they registered.

use std::io::{Read as _, Write as _};
use std::time::Duration;

/// Result of a liveness check on a registered session name.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum Liveness {
    /// A psmux server answers on the registered port (or its registered
    /// PID is still a running psmux process — busy, but alive).
    Alive,
    /// Registration exists but no server answers and the PID is gone.
    Dead,
    /// No port file for this name.
    Missing,
}

/// Outcome of a fresh server's attempt to register its name.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum RegisterOutcome {
    Registered,
    /// A live server already owns this name.
    NameTaken,
}

/// Result of a server checking the registry entry for its own session.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum OwnRegistrationAudit {
    /// The `<base>.port` file still points at this server.
    Current,
    /// The registration was missing and this process recreated it.
    Repaired,
    /// The name is missing and could not be reclaimed.
    Missing,
    /// The name now points at another server.
    OwnedByOther,
}

pub fn psmux_dir() -> String {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    format!("{}\\.psmux", home)
}

pub fn port_path(base: &str) -> String { format!("{}\\{}.port", psmux_dir(), base) }
pub fn key_path(base: &str) -> String { format!("{}\\{}.key", psmux_dir(), base) }
pub fn pid_path(base: &str) -> String { format!("{}\\{}.pid", psmux_dir(), base) }

pub fn log_registry_event(event: &str, detail: &str) {
    let dir = psmux_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{}\\registry.log", dir))
    {
        let _ = writeln!(
            f,
            "[{}][pid {}][{}] {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            std::process::id(),
            event,
            detail
        );
        let _ = f.flush();
    }
}

pub fn read_port(base: &str) -> Option<u16> {
    std::fs::read_to_string(port_path(base)).ok()?.trim().parse().ok()
}

pub fn read_pid(base: &str) -> Option<u32> {
    std::fs::read_to_string(pid_path(base)).ok()?.trim().parse().ok()
}

/// TCP probe: connect and exchange an AUTH line to confirm a *psmux server*
/// answers on `port`.  A successful connect alone is not proof — the port
/// may have been recycled by an unrelated app — so require the psmux AUTH
/// response banner.  AUTH is handled by the per-connection thread, so this
/// succeeds even while the server's main loop is busy.
pub fn probe_psmux_server(port: u16, connect_timeout: Duration) -> bool {
    let addr: std::net::SocketAddr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let mut s = match std::net::TcpStream::connect_timeout(&addr, connect_timeout) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    if s.write_all(b"AUTH __liveness_probe__\n").is_err() { return false; }
    let _ = s.flush();
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) if n > 0 => {
            let resp = String::from_utf8_lossy(&buf[..n]);
            // Real servers answer "OK" (valid key) or the invalid-key error.
            resp.starts_with("OK") || resp.contains("Invalid session key")
        }
        _ => false,
    }
}

/// Check whether `pid` is a live process running a psmux-family image.
/// Matches tmux/psmux/pmux including deploy-renamed images (tmux.exe.old),
/// so servers surviving a binary swap are still recognized.
#[cfg(windows)]
pub fn pid_is_live_psmux(pid: u32) -> bool {
    const TH32CS_SNAPPROCESS: u32 = 0x00000002;
    const INVALID_HANDLE: isize = -1;

    #[repr(C)]
    struct PROCESSENTRY32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> isize;
        fn Process32FirstW(h_snapshot: isize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn Process32NextW(h_snapshot: isize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE || snap == 0 { return false; }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dw_size = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                if pe.th32_process_id == pid {
                    let len = pe.sz_exe_file.iter().position(|&c| c == 0).unwrap_or(260);
                    let name = String::from_utf16_lossy(&pe.sz_exe_file[..len]).to_lowercase();
                    found = name.starts_with("tmux") || name.starts_with("psmux") || name.starts_with("pmux");
                    break;
                }
                if Process32NextW(snap, &mut pe) == 0 { break; }
            }
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(not(windows))]
pub fn pid_is_live_psmux(pid: u32) -> bool {
    // Linux: /proc/<pid>/comm names the image directly.
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{}/comm", pid)) {
        let c = comm.trim().to_lowercase();
        return c.contains("mux");
    }
    // Other Unix (no procfs): existence check via kill -0.
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Full liveness verdict for a registered name.
///
/// The TCP probe is retried with rising timeouts so the common healthy case
/// stays fast (a dead port refuses instantly on loopback) while load spikes
/// and post-sleep page-in stalls get time to clear.  If all probes fail,
/// the registered PID gets the final word: a running psmux process keeps
/// its registration even while unresponsive.
pub fn registration_liveness(base: &str) -> Liveness {
    let Some(port) = read_port(base) else { return Liveness::Missing };
    for timeout_ms in [150u64, 400, 1200] {
        if probe_psmux_server(port, Duration::from_millis(timeout_ms)) {
            return Liveness::Alive;
        }
    }
    match read_pid(base) {
        Some(pid) if pid_is_live_psmux(pid) => {
            log_registry_event(
                "liveness_alive_by_pid",
                &format!("base={} port={} pid={} tcp_probe_failed=true", base, port, pid),
            );
            Liveness::Alive
        }
        Some(pid) => {
            log_registry_event(
                "liveness_dead",
                &format!("base={} port={} pid={} tcp_probe_failed=true", base, port, pid),
            );
            Liveness::Dead
        }
        // Registration from a build predating .pid files: the process can't
        // be verified, and three probes failed — treat as dead (matches the
        // old behavior, but only after real retries instead of one 100ms shot).
        None => {
            log_registry_event(
                "liveness_dead_no_pid",
                &format!("base={} port={} tcp_probe_failed=true", base, port),
            );
            Liveness::Dead
        }
    }
}

/// Remove a registration decided to be stale.  Callers must have obtained a
/// `Liveness::Dead` verdict (or otherwise own the name) first.
pub fn remove_registration(base: &str) {
    let old_port = read_port(base);
    let old_pid = read_pid(base);
    log_registry_event(
        "remove_registration",
        &format!("base={} old_port={:?} old_pid={:?}", base, old_port, old_pid),
    );
    let _ = std::fs::remove_file(port_path(base));
    let _ = std::fs::remove_file(key_path(base));
    let _ = std::fs::remove_file(pid_path(base));
}

/// Remove the registration for `base` ONLY if the port file still points at
/// `my_port`.  Servers tearing down must use this so a name that was taken
/// over by a newer server is never deregistered by its previous owner.
/// Returns true when the files were removed.
pub fn remove_registration_if_owner(base: &str, my_port: u16) -> bool {
    match read_port(base) {
        Some(p) if p == my_port => {
            log_registry_event(
                "remove_registration_if_owner",
                &format!("base={} port={}", base, my_port),
            );
            remove_registration(base);
            true
        }
        other => {
            log_registry_event(
                "skip_remove_registration_if_owner",
                &format!("base={} my_port={} registered_port={:?}", base, my_port, other),
            );
            false
        }
    }
}

/// Register a fresh server under `base`.  The atomic `create_new` of the
/// port file is the claim: when the name is already registered, defer to a
/// live owner and take over a dead one.
pub fn register_new_server(base: &str, port: u16, key: &str) -> RegisterOutcome {
    let dir = psmux_dir();
    let _ = std::fs::create_dir_all(&dir);
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(port_path(base)) {
            Ok(mut f) => {
                let _ = f.write_all(port.to_string().as_bytes());
                let _ = std::fs::write(key_path(base), key);
                let _ = std::fs::write(pid_path(base), std::process::id().to_string());
                log_registry_event(
                    "register_new_server",
                    &format!("base={} port={} outcome=registered", base, port),
                );
                return RegisterOutcome::Registered;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match registration_liveness(base) {
                    Liveness::Alive => {
                        log_registry_event(
                            "register_new_server",
                            &format!("base={} port={} outcome=name_taken", base, port),
                        );
                        return RegisterOutcome::NameTaken;
                    }
                    _ => {
                        log_registry_event(
                            "register_new_server",
                            &format!("base={} port={} outcome=stale_remove", base, port),
                        );
                        remove_registration(base)
                    }, // stale — clear and retry the claim
                }
            }
            Err(_) => {
                // Filesystem error (permissions, transient lock): fall back
                // to the historical unconditional write rather than failing
                // session creation outright.
                let _ = std::fs::write(port_path(base), port.to_string());
                let _ = std::fs::write(key_path(base), key);
                let _ = std::fs::write(pid_path(base), std::process::id().to_string());
                log_registry_event(
                    "register_new_server",
                    &format!("base={} port={} outcome=fallback_registered", base, port),
                );
                return RegisterOutcome::Registered;
            }
        }
    }
    // Lost the claim race twice — someone else registered and is alive.
    RegisterOutcome::NameTaken
}

/// Rename this server's registration from `old_base` to `new_base`
/// (rename-session / warm claim).  Refuses when `new_base` is registered to
/// a live server; takes over a dead registration.  `key` is the server's
/// in-memory session key — deliberately not read back from the old key
/// file, which may already belong to a usurper.
pub fn move_registration(old_base: &str, new_base: &str, my_port: u16, key: &str) -> Result<(), String> {
    if new_base != old_base {
        match registration_liveness(new_base) {
            Liveness::Alive => {
                log_registry_event(
                    "move_registration",
                    &format!(
                        "old_base={} new_base={} port={} outcome=name_taken",
                        old_base, new_base, my_port
                    ),
                );
                return Err(format!("session name '{}' already in use", new_base));
            }
            Liveness::Dead => remove_registration(new_base),
            Liveness::Missing => {}
        }
    }
    remove_registration_if_owner(old_base, my_port);
    let _ = std::fs::write(port_path(new_base), my_port.to_string());
    let _ = std::fs::write(key_path(new_base), key);
    let _ = std::fs::write(pid_path(new_base), std::process::id().to_string());
    log_registry_event(
        "move_registration",
        &format!(
            "old_base={} new_base={} port={} outcome=moved",
            old_base, new_base, my_port
        ),
    );
    Ok(())
}

/// Check this process's own registration and repair the exact failure mode
/// where a stale cleanup deleted `<base>.port` / `<base>.key` while leaving
/// the live server process attached to existing clients.
pub fn audit_or_repair_own_registration(base: &str, port: u16, key: &str) -> OwnRegistrationAudit {
    match read_port(base) {
        Some(p) if p == port => OwnRegistrationAudit::Current,
        Some(p) => {
            log_registry_event(
                "own_registration_owned_by_other",
                &format!("base={} my_port={} registered_port={}", base, port, p),
            );
            OwnRegistrationAudit::OwnedByOther
        }
        None => {
            let pid = read_pid(base);
            if matches!(pid, Some(existing) if existing != std::process::id()) {
                log_registry_event(
                    "own_registration_missing_port_pid_mismatch",
                    &format!(
                        "base={} my_port={} registered_pid={:?}",
                        base, port, pid
                    ),
                );
                return OwnRegistrationAudit::Missing;
            }

            match std::fs::OpenOptions::new().write(true).create_new(true).open(port_path(base)) {
                Ok(mut f) => {
                    let _ = f.write_all(port.to_string().as_bytes());
                    let _ = std::fs::write(key_path(base), key);
                    let _ = std::fs::write(pid_path(base), std::process::id().to_string());
                    log_registry_event(
                        "own_registration_repaired",
                        &format!("base={} port={} previous_pid={:?}", base, port, pid),
                    );
                    OwnRegistrationAudit::Repaired
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    log_registry_event(
                        "own_registration_repair_race_lost",
                        &format!("base={} port={} previous_pid={:?}", base, port, pid),
                    );
                    OwnRegistrationAudit::OwnedByOther
                }
                Err(e) => {
                    log_registry_event(
                        "own_registration_repair_failed",
                        &format!("base={} port={} previous_pid={:?} err={}", base, port, pid, e),
                    );
                    OwnRegistrationAudit::Missing
                }
            }
        }
    }
}
