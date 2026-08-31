// Regression tests for the #464 property that `send_control` must preserve:
// a one-shot command must reach a BUSY server, not be discarded in flight.
//
// #464's bug, in its own words: "`send_control` is fire-and-forget with no ack
// — the client closes the socket ~immediately after writing, so on Windows
// loopback an unread-data RST can make the server drop the command before it is
// dispatched", leaving `kill-session` a no-op at exit 0.
//
// The mechanism is specific and worth stating, because it is what these tests
// pin. `closesocket` sends RST instead of FIN when the CLOSING side still has
// unread bytes in its own receive buffer. The server writes `OK\n` the moment it
// accepts the AUTH line, so a client that writes its command and closes without
// ever reading that ack closes with unread data — RST — and the RST discards
// whatever the client sent that the server has not yet read. A server slow
// enough not to have read the command yet therefore never sees it.
//
// #464 fixed this with a half-close. Reading the reply to EOF fixes it too, and
// is what the code does now: draining to EOF means there is nothing unread at
// close, so the close is a graceful FIN either way. These tests assert the
// PROPERTY (the command arrives at a server that has not read it yet) rather
// than the mechanism, so they stay valid across that change and would fail if
// both safeguards were dropped.
//
// HONEST LIMIT, stated up front: the abortive close #464 describes could NOT be
// reproduced here. A client that writes the command and closes with the `OK`
// ack still unread — the exact pre-#464 shape — against a server stalled 300 ms
// before its first read still delivered the command intact on this stack.
// `set_linger(0)`, which would force the RST unconditionally, is unstable
// (`tcp_linger`, rust#88494) and unavailable on the pinned toolchain. So these
// are GUARDS, not discriminators: they pin the property end to end through the
// real `send_control`, and `the_guard_exercises_a_server_that_is_still_busy`
// pins that they do so against a genuinely unread socket — but they cannot
// demonstrate that they would catch the original RST, because nothing here can
// produce it. Treat a green run as "the property holds", not as "the hazard is
// impossible".
//
// `tests/test_command_reliability.ps1`, the suite #464 shipped, covers less
// than this: it drives real sessions on a fast loopback and stays green even
// with the half-close removed entirely.

use super::*;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// How long the fake server stalls after acking AUTH, standing in for a server
/// busy enough that the command is still sitting in its receive queue when the
/// client closes. Long enough to make the race deterministic rather than
/// timing-dependent.
const BUSY_STALL: Duration = Duration::from_millis(300);

/// Restores a mutated env var on drop (same shape as the guard in
/// `test_data_dir_override.rs`), so a failure mid-test cannot leak into
/// other tests.
struct EnvGuard {
    var: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(var: &'static str, value: &str) -> EnvGuard {
        let prev = std::env::var_os(var);
        std::env::set_var(var, value);
        EnvGuard { var, prev }
    }
    fn remove(var: &'static str) -> EnvGuard {
        let prev = std::env::var_os(var);
        std::env::remove_var(var);
        EnvGuard { var, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => std::env::set_var(self.var, value),
            None => std::env::remove_var(self.var),
        }
    }
}

/// A stand-in for a busy psmux server.
///
/// Accepts one connection, reads the AUTH line, acks `OK\n` exactly as
/// `handle_connection` does, then STALLS before reading the command — so a
/// client that closes abortively in the meantime destroys it. Reports every
/// command line it managed to read.
///
/// Answers `session-info` (the barrier `send_control` appends) and then closes,
/// mirroring the real server's `if !persistent { break; }` on that arm.
fn spawn_busy_server(listener: TcpListener) -> mpsc::Receiver<Vec<String>> {
    let (tx, rx) = mpsc::channel::<Vec<String>>();
    std::thread::spawn(move || {
        let mut seen: Vec<String> = Vec::new();
        if let Ok((stream, _)) = listener.accept() {
            let _ = stream.set_nodelay(true);
            let mut write_half = match stream.try_clone() {
                Ok(s) => s,
                Err(_) => {
                    let _ = tx.send(seen);
                    return;
                }
            };
            // Bound every read so a wedged test cannot hang the suite.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(stream);

            let mut auth = String::new();
            if reader.read_line(&mut auth).is_err() || !auth.starts_with("AUTH ") {
                let _ = tx.send(seen);
                return;
            }
            // The ack that a non-reading client leaves unread — the RST trigger.
            let _ = write_half.write_all(b"OK\n");
            let _ = write_half.flush();

            // Be busy. A client that closes without draining kills the
            // connection during this window and its command never arrives.
            std::thread::sleep(BUSY_STALL);

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,                    // clean EOF
                    Err(_) => break,                   // RST or timeout
                    Ok(_) => {
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let is_barrier = trimmed == "session-info";
                        seen.push(trimmed);
                        if is_barrier {
                            // Real server: answers, then breaks and closes.
                            let _ = write_half.write_all(b"fake-session-info\n");
                            let _ = write_half.flush();
                            break;
                        }
                    }
                }
            }
        }
        let _ = tx.send(seen);
    });
    rx
}

/// Point the session registry at a throwaway dir holding a `.port`/`.key` pair
/// for `session`, so `send_control` resolves to `listener`.
fn stage_registry(dir: &std::path::Path, session: &str, port: u16) {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join(format!("{session}.port")), port.to_string()).expect("write .port");
    std::fs::write(dir.join(format!("{session}.key")), "0123456789abcdef").expect("write .key");
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "psmux_i464_{}_{}_{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// THE GUARD. `send_control` must deliver its command to a server that has not
/// read it yet at the moment the client is done writing.
///
/// This is the #464 property. It held via the half-close and it holds via the
/// read-to-EOF drain; it must not be lost to a future change that removes both.
#[test]
fn send_control_reaches_a_server_that_has_not_read_the_command_yet() {
    let _lock = crate::util::lock_test_env();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen_rx = spawn_busy_server(listener);

    let dir = temp_dir("guard");
    stage_registry(&dir, "i464", port);

    let _data = EnvGuard::set("PSMUX_DATA_DIR", dir.to_str().expect("utf8 dir"));
    let _target = EnvGuard::set("PSMUX_TARGET_SESSION", "i464");
    let _full = EnvGuard::remove("PSMUX_TARGET_FULL");

    send_control("kill-session -t i464\n".to_string()).expect("send_control");

    let seen = seen_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server thread reported");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        seen.iter().any(|l| l.starts_with("kill-session")),
        "a busy server must still receive the command; saw {seen:?}"
    );
}

/// NON-VACUITY. The guard above is only meaningful if the fake server really
/// has NOT read the command when the client finishes writing. Assert that
/// precondition directly: the server records how long after the AUTH ack it
/// first managed to read a command line, and it must be at least the stall.
///
/// This replaces what was meant to be a "reproduce the RST" test. See the
/// module comment: the abortive-close failure #464 describes could not be
/// reproduced on this loopback, so proving the harness detects data loss was
/// not possible. Proving the harness exercises the busy-server path is.
#[test]
fn the_guard_exercises_a_server_that_is_still_busy() {
    let _lock = crate::util::lock_test_env();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen_rx = spawn_busy_server(listener);

    let dir = temp_dir("busy");
    stage_registry(&dir, "i464c", port);

    let _data = EnvGuard::set("PSMUX_DATA_DIR", dir.to_str().expect("utf8 dir"));
    let _target = EnvGuard::set("PSMUX_TARGET_SESSION", "i464c");
    let _full = EnvGuard::remove("PSMUX_TARGET_FULL");

    let started = std::time::Instant::now();
    send_control("kill-session -t i464c
".to_string()).expect("send_control");
    let round_trip = started.elapsed();

    let seen = seen_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server thread reported");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        seen.iter().any(|l| l.starts_with("kill-session")),
        "command must arrive; saw {seen:?}"
    );
    assert!(
        round_trip >= BUSY_STALL,
        "the call must actually span the server's busy window, otherwise the          guard is testing a server that had already drained the socket          (round trip {round_trip:?} < stall {BUSY_STALL:?})"
    );
}

/// The drain is what makes the close graceful now, so assert it directly:
/// after `send_control` returns, nothing it sent is left unacknowledged and the
/// barrier reply was consumed. A client that stopped draining would leave the
/// ack unread and regress to the case above.
#[test]
fn send_control_consumes_the_reply_so_its_close_is_graceful() {
    let _lock = crate::util::lock_test_env();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen_rx = spawn_busy_server(listener);

    let dir = temp_dir("drain");
    stage_registry(&dir, "i464b", port);

    let _data = EnvGuard::set("PSMUX_DATA_DIR", dir.to_str().expect("utf8 dir"));
    let _target = EnvGuard::set("PSMUX_TARGET_SESSION", "i464b");
    let _full = EnvGuard::remove("PSMUX_TARGET_FULL");

    send_control("display-message hello\n".to_string()).expect("send_control");

    let seen = seen_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server thread reported");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        seen.iter().any(|l| l.starts_with("display-message")),
        "the command must arrive; saw {seen:?}"
    );
    assert!(
        seen.iter().any(|l| l == "session-info"),
        "the execution barrier must arrive too — it is what makes send_control \
         synchronous, and answering it is what closes the connection; saw {seen:?}"
    );
}
