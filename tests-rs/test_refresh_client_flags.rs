// Regression tests for the refresh-client control-only flag rejection
// (f62b3cb + d80093a).
//
// refresh-client -C/-B/-A/-f only make sense for a control-mode client;
// tmux rejects them with "not a control client" for anyone else. The
// one-shot CLI path used to silently drop every flag, letting callers
// believe a size or subscription was applied when nothing happened. The
// fix rejects the flags server-side with tmux's error text, which the CLI
// then surfaces (error to stderr, non-zero exit).
//
// The server-side rejection lives in `handle_connection`, the one-shot CLI
// connection handler. These tests drive it over real loopback sockets and
// assert the exact wire text for every control-only flag, plus that a
// flag-free refresh-client is still forwarded to the server loop.
//
// They deliberately do NOT half-close the write side, because the real
// one-shot client no longer does either (see
// `session::send_control_with_response`): end-of-reply is the server's own
// close. refresh-client is a handler that answers without breaking out of the
// command loop, so it reaches that close only via the loop-tail batch read
// timing out — exactly the path these tests need to cover. Half-closing here
// would end the read through the retired client-FIN path, and the assertions
// would still pass even if the loop-tail close regressed.

use super::*;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

const TEST_KEY: &str = "unit-test-key";

/// Serve one connection through the real one-shot handler.
fn spawn_handler(listener: TcpListener, tx: mpsc::Sender<CtrlReq>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("handler: accept");
        handle_connection(
            stream,
            tx,
            TEST_KEY,
            Arc::new(RwLock::new(std::collections::HashMap::new())),
        );
    })
}

/// Connect like the one-shot CLI client: AUTH, expect OK, return the socket.
fn connect_authenticated(addr: std::net::SocketAddr) -> TcpStream {
    let mut client = TcpStream::connect(addr).expect("client connect");
    client.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(client, "AUTH {TEST_KEY}\n").unwrap();
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    let mut ok = String::new();
    reader.read_line(&mut ok).expect("read OK");
    assert_eq!(ok, "OK\n", "handler must authenticate the client");
    client
}

/// Every control-only refresh-client flag is rejected on the one-shot CLI
/// path with tmux's "not a control client" error carrying the flag name —
/// the text the CLI propagates to its exit path.
#[test]
fn control_only_flags_are_rejected_with_the_tmux_error_text() {
    for flag in ["-C", "-B", "-A", "-f"] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let (tx, _rx) = mpsc::channel::<CtrlReq>();
        let handler = spawn_handler(listener.try_clone().unwrap(), tx);

        let mut client = connect_authenticated(listener.local_addr().unwrap());
        write!(client, "refresh-client {flag}\n").unwrap();

        let mut resp = String::new();
        client.read_to_string(&mut resp).expect("read response");
        handler.join().expect("handler thread");

        assert_eq!(
            resp,
            format!("ERROR: refresh-client {flag}: not a control client\n"),
            "the one-shot CLI path must reject {flag} instead of silently dropping it"
        );
    }
}

/// A refresh-client without control-only flags is still forwarded to the
/// server loop (the rejection must not break the flag-free path).
#[test]
fn flag_free_refresh_client_is_forwarded_to_the_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let (tx, rx) = mpsc::channel::<CtrlReq>();
    let handler = spawn_handler(listener.try_clone().unwrap(), tx);

    let mut client = connect_authenticated(listener.local_addr().unwrap());
    write!(client, "refresh-client\n").unwrap();

    let mut resp = String::new();
    client.read_to_string(&mut resp).expect("read response");
    handler.join().expect("handler thread");

    assert_eq!(resp, "", "flag-free refresh-client must produce no error");
    let request = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the request must reach the server loop");
    assert!(
        matches!(request, CtrlReq::RefreshClient),
        "flag-free refresh-client must be forwarded unchanged"
    );
}

/// Unknown flags are not control-only and must not be rejected: the
/// rejection must key off exactly -C/-B/-A/-f.
#[test]
fn non_control_flags_are_not_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let (tx, rx) = mpsc::channel::<CtrlReq>();
    let handler = spawn_handler(listener.try_clone().unwrap(), tx);

    let mut client = connect_authenticated(listener.local_addr().unwrap());
    write!(client, "refresh-client -S\n").unwrap();

    let mut resp = String::new();
    client.read_to_string(&mut resp).expect("read response");
    handler.join().expect("handler thread");

    assert_eq!(resp, "", "-S must not be rejected as control-only");
    let request = rx.recv_timeout(Duration::from_secs(2)).expect("request forwarded");
    assert!(matches!(request, CtrlReq::RefreshClient));
}
