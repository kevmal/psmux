// Tests for the two properties `send_control` must hold simultaneously:
//
//   1. DELIVERY (#464). A one-shot command must reach a BUSY server — one that
//      has not yet read it when the client finishes writing — rather than being
//      discarded in flight.
//   2. PASSIVE CLOSE. The client must not be the side that closes first, or it
//      parks an ephemeral port in TIME_WAIT on every call.
//
// They pull in opposite directions, which is why both are pinned here. #464's
// fix bought (1) with a client-side half-close, which cost (2). The half-close
// is gone; (1) now rests on the drain-to-EOF below the write, and (2) follows.
//
// #464's mechanism, since it is what these tests are about: `closesocket` sends
// RST instead of FIN when the CLOSING side still has unread bytes in its own
// receive buffer. The server acks `OK\n` the moment it reads the AUTH line, so
// a client that writes its command and closes without reading that ack closes
// with unread data, and the RST discards whatever the server has not yet read —
// including the command. Draining to EOF means nothing is unread at close, so
// the close is a graceful FIN whether or not a FIN was sent earlier. The command
// is delimited by its newline, not by EOF, so the server never needed the FIN in
// order to dispatch.
//
// HONEST LIMIT — read before trusting a green run. The abortive close #464
// describes could NOT be reproduced here. A client in the exact pre-#464 shape
// (write, then close with the ack unread, no half-close) against a server
// stalled 300 ms before its first read still delivered the command intact on
// this loopback. `set_linger(0)`, which would force the RST unconditionally, is
// unstable (`tcp_linger`, rust#88494) and unavailable on the pinned toolchain.
//
// So `send_control_reaches_a_server_that_has_not_read_the_command_yet` is a
// GUARD for the delivery property, not a reproduction of the hazard. It cannot
// be claimed to catch every way of losing (1): in particular these tests would
// very likely stay green even if the drain were removed, because the hazard that
// would then bite does not fire on this stack. Do not read a green run as
// licence to remove the drain.
//
// `send_control_does_not_close_the_connection_first` is different in kind: it is
// a true discriminator and fails if the half-close is restored.
//
// `tests/test_command_reliability.ps1`, the suite #464 shipped, covers less than
// any of this — it drives real sessions on a fast loopback and stays green with
// the half-close removed entirely.

use super::*;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the fake server stalls after acking AUTH, standing in for a server
/// busy enough that the command is still unread when the client stops writing.
const BUSY_STALL: Duration = Duration::from_millis(300);

/// How long the server waits, after answering the barrier, to see whether the
/// client closes its write side. A client that half-closes shows up as a clean
/// EOF well inside this window; one that waits for the server shows nothing.
const FIN_PROBE: Duration = Duration::from_millis(250);

/// Restores a mutated env var on drop, so a failure mid-test cannot leak the
/// value into other tests.
///
/// NOTE: this is the fourth copy of this shape in `tests-rs` (see
/// `test_data_dir_override.rs`, `test_config_plugin_paths.rs`,
/// `test_issue599_data_root_mutex.rs`). Hoisting one copy next to
/// `crate::util::lock_test_env` is a worthwhile follow-up; duplicated here
/// rather than refactoring four call sites inside an unrelated fix.
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

/// What the fake server actually observed. Server-side instrumentation, so the
/// assertions rest on what the server saw rather than on a client-side proxy.
struct Observed {
    /// Every non-empty command line read, in order.
    seen: Vec<String>,
    /// Delay between acking AUTH and successfully reading the first command
    /// line. Proves the command really was still unread during the stall.
    first_read_after_ack: Option<Duration>,
    /// True when the client closed its write side before the server closed —
    /// i.e. the client was the active closer. This is what a half-close looks
    /// like from the far end.
    client_closed_first: bool,
}

/// A stand-in for a busy psmux server.
///
/// Accepts one connection, reads AUTH, acks `OK\n` exactly as
/// `handle_connection` does, then STALLS before its first read. Answers the
/// `session-info` barrier (mirroring the real server's `if !persistent
/// { break; }` on that arm), then probes for a client FIN before closing.
fn spawn_busy_server(listener: TcpListener) -> mpsc::Receiver<Observed> {
    let (tx, rx) = mpsc::channel::<Observed>();
    std::thread::spawn(move || {
        let mut obs = Observed {
            seen: Vec::new(),
            first_read_after_ack: None,
            client_closed_first: false,
        };
        if let Ok((stream, _)) = listener.accept() {
            let _ = stream.set_nodelay(true);
            let mut write_half = match stream.try_clone() {
                Ok(s) => s,
                Err(_) => {
                    let _ = tx.send(obs);
                    return;
                }
            };
            // Bound every read so a wedged test cannot hang the suite.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(stream);

            let mut auth = String::new();
            if reader.read_line(&mut auth).is_err() || !auth.starts_with("AUTH ") {
                let _ = tx.send(obs);
                return;
            }
            // The ack a non-reading client would leave unread — the RST trigger.
            let _ = write_half.write_all(b"OK\n");
            let _ = write_half.flush();
            let acked_at = Instant::now();

            // Be busy: the command sits unread in the receive queue throughout.
            std::thread::sleep(BUSY_STALL);

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,  // clean EOF
                    Err(_) => break, // reset or timeout
                    Ok(_) => {
                        if obs.first_read_after_ack.is_none() {
                            obs.first_read_after_ack = Some(acked_at.elapsed());
                        }
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let is_barrier = trimmed == "session-info";
                        obs.seen.push(trimmed);
                        if is_barrier {
                            let _ = write_half.write_all(b"fake-session-info\n");
                            let _ = write_half.flush();
                            // The real server breaks here and closes. Before
                            // closing, look for a client FIN: if the client
                            // half-closed, its EOF is already queued and this
                            // read returns Ok(0) immediately. If the client is
                            // waiting on us instead, this read times out.
                            let _ = reader.get_ref().set_read_timeout(Some(FIN_PROBE));
                            let mut probe = String::new();
                            obs.client_closed_first =
                                matches!(reader.read_line(&mut probe), Ok(0));
                            break;
                        }
                    }
                }
            }
        }
        let _ = tx.send(obs);
    });
    rx
}

/// Point the session registry at a throwaway dir holding a `.port`/`.key` pair
/// for `session`, so `send_control` resolves to our fake listener.
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

/// Run `send_control(line)` against a busy fake server and report what the
/// server saw. Serializes on the crate-wide env lock because it mutates
/// process-global env.
fn send_control_against_busy_server(tag: &str, session: &str, line: &str) -> Observed {
    let _lock = crate::util::lock_test_env();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let rx = spawn_busy_server(listener);

    let dir = temp_dir(tag);
    stage_registry(&dir, session, port);

    let _data = EnvGuard::set("PSMUX_DATA_DIR", dir.to_str().expect("utf8 dir"));
    let _target = EnvGuard::set("PSMUX_TARGET_SESSION", session);
    let _full = EnvGuard::remove("PSMUX_TARGET_FULL");

    send_control(line.to_string()).expect("send_control");

    let obs = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server thread reported");
    let _ = std::fs::remove_dir_all(&dir);
    obs
}

/// DELIVERY GUARD (#464). The command must reach a server that had not read it
/// when the client finished writing — and the server-side timing must confirm
/// the test really exercised that window, or the guard proves nothing.
#[test]
fn send_control_reaches_a_server_that_has_not_read_the_command_yet() {
    let obs = send_control_against_busy_server("guard", "i464", "kill-session -t i464\n");

    assert!(
        obs.seen.iter().any(|l| l.starts_with("kill-session")),
        "a busy server must still receive the command; saw {:?}",
        obs.seen
    );
    // Non-vacuity, measured server-side: the first successful read happened only
    // after the stall, so the command genuinely sat unread meanwhile.
    let first = obs
        .first_read_after_ack
        .expect("server must have read at least one line");
    assert!(
        first >= BUSY_STALL,
        "the guard must exercise a server that had NOT drained the socket; first \
         read came {first:?} after the ack, before the {BUSY_STALL:?} stall"
    );
}

/// THE DISCRIMINATOR for this change. The client must not be the side that
/// closes first.
///
/// Unlike the delivery guard, this test genuinely fails if the fix is reverted:
/// restoring `shutdown(Shutdown::Write)` in `send_control` makes the client's
/// EOF arrive before the server closes, which the server sees as `Ok(0)` on the
/// probe read. That is what keeps the ephemeral-port fix from regressing
/// silently, detectable only by re-running a manual netstat measurement.
#[test]
fn send_control_does_not_close_the_connection_first() {
    let obs = send_control_against_busy_server("fin", "i464fin", "display-message hi\n");

    assert!(
        obs.seen.iter().any(|l| l.starts_with("display-message")),
        "precondition: the command must arrive; saw {:?}",
        obs.seen
    );
    assert!(
        !obs.client_closed_first,
        "send_control must let the SERVER close first — a client-side FIN makes \
         the client the active closer and parks one ephemeral port in TIME_WAIT \
         per call, which is the leak this exists to prevent"
    );
}

/// The execution barrier must arrive too: it is what makes `send_control`
/// synchronous, and answering it is what prompts the server to close.
#[test]
fn send_control_delivers_the_execution_barrier() {
    let obs = send_control_against_busy_server("barrier", "i464b", "display-message hello\n");

    assert!(
        obs.seen.iter().any(|l| l.starts_with("display-message")),
        "the command must arrive; saw {:?}",
        obs.seen
    );
    assert!(
        obs.seen.iter().any(|l| l == "session-info"),
        "the execution barrier must arrive as its own command; saw {:?}",
        obs.seen
    );
}

/// A caller that omits the trailing newline must not have its command fused
/// with the appended `session-info` barrier.
///
/// Without normalization the wire carries `display-message hisession-info\n`:
/// one corrupted command, the barrier eaten as payload, and `Ok(())` returned
/// while nothing the caller asked for ran. This pre-dates the half-close removal
/// — the two writes were always adjacent — but it is silent, and this is a
/// `pub fn`.
#[test]
fn send_control_normalizes_a_missing_trailing_newline() {
    let obs = send_control_against_busy_server("nl", "i464nl", "display-message hi");

    assert!(
        obs.seen.iter().any(|l| l == "display-message hi"),
        "the command must arrive intact, not fused with the barrier; saw {:?}",
        obs.seen
    );
    assert!(
        obs.seen.iter().any(|l| l == "session-info"),
        "the barrier must survive as its own command; saw {:?}",
        obs.seen
    );
}
