// Regression guard for pane-id delivery of the send commands.
//
// `send-keys -t %N` (and send-paste / send-text / send-key) used to be two
// requests on the server channel every client shares: a temporary focus
// switch, then a write to whatever pane was active. Any other client's
// request landing in between restored or re-pointed the focus, and the keys
// went to a window the caller never named. On 2026-09-01 that typed one
// orchestrator's message into another agent's prompt, three sessions over,
// while every capture (already resolved by id) kept showing the right pane.
//
// These pin the replacement: delivery is resolved by pane id inside the
// request, writes nothing anywhere for an unknown id, never moves focus, and
// hands the active pane to the mode-aware route it always had. A dummy PTY
// (no spawn) with an inspectable writer keeps them hermetic on every host.

use super::*;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::server::send_keys::{deliver_send_keys, SendSink};
use crate::types::Node;

const ROWS: u16 = 6;
const COLS: u16 = 40;

// ── PTY-free pane scaffolding, with a writer the test can read back ────────

#[derive(Debug)]
struct DummyChild;

struct DummyMaster;

/// Everything written to the pane, so a test can assert on delivery.
#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> { self.0.lock().expect("writer lock").clone() }
}

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("writer lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

impl portable_pty::ChildKiller for DummyChild {
    fn kill(&mut self) -> std::io::Result<()> { Ok(()) }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(DummyChild)
    }
}

impl portable_pty::Child for DummyChild {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
    }
    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        Ok(portable_pty::ExitStatus::with_exit_code(0))
    }
    fn process_id(&self) -> Option<u32> { None }
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> { None }
}

impl portable_pty::MasterPty for DummyMaster {
    fn resize(&self, _size: portable_pty::PtySize) -> Result<(), anyhow::Error> { Ok(()) }
    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        Ok(portable_pty::PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
    }
    fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, anyhow::Error> {
        Ok(Box::new(std::io::empty()))
    }
    fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, anyhow::Error> {
        Ok(Box::new(SharedWriter::default()))
    }
    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32> { None }
    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> { None }
    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> { None }
}

fn make_pane(id: usize) -> (crate::types::Pane, SharedWriter) {
    let writer = SharedWriter::default();
    let term = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
    let epoch = Instant::now() - Duration::from_secs(2);
    let pane = crate::types::Pane {
        master: Box::new(DummyMaster),
        writer: Box::new(writer.clone()),
        child: Box::new(DummyChild),
        term,
        last_rows: ROWS,
        last_cols: COLS,
        id,
        title: format!("pane{id}"),
        title_locked: false,
        child_pid: None,
        data_version: Arc::new(AtomicU64::new(0)),
        last_title_check: epoch,
        last_infer_title: epoch,
        dead: false,
        last_text_input: None,
        last_special_key: None,
        vt_bridge_cache: None,
        vti_mode_cache: None,
        mouse_input_cache: None,
        scroll_fg_cache: None, mouse_proto_owner: None, wheel_auth: None,
        cursor_shape: Arc::new(AtomicU8::new(0)),
        bell_pending: Arc::new(AtomicBool::new(false)),
        cpr_pending: Arc::new(AtomicBool::new(false)),
        color_query_pending: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        copy_state: None,
        pane_style: None, pane_options: Default::default(),
        squelch_until: None,
        output_ring: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        spawned_at: None,
    };
    (pane, writer)
}

fn make_window(id: usize, pane: crate::types::Pane) -> crate::types::Window {
    crate::types::Window {
        root: Node::Leaf(pane),
        active_path: vec![],
        name: format!("w{id}"),
        id,
        area: ratatui::layout::Rect::new(0, 0, 120, 30),
        window_size: None,
        activity_flag: false,
        bell_flag: false,
        silence_flag: false,
        last_output_time: Instant::now(),
        last_seen_version: 0,
        manual_rename: false,
        layout_index: 0,
        pane_mru: vec![],
        zoom_saved: None,
        linked_from: None,
        floating: Vec::new(),
        floating_focus: None,
    }
}

/// Two windows, one pane each. Window 0 (pane %0) is active; pane %1 lives in
/// window 1 and is the `-t %1` target that is NOT the active pane — the shape
/// of every agent window in a shared orchestration session.
fn app_two_windows() -> (AppState, SharedWriter, SharedWriter) {
    let mut app = AppState::new("sendbyid".to_string());
    app.window_base_index = 0;
    app.pane_base_index = 0;
    app.copy_command = String::new();
    app.set_clipboard = "off".to_string();
    let (pane0, w0) = make_pane(0);
    let (pane1, w1) = make_pane(1);
    app.windows.push(make_window(0, pane0));
    app.windows.push(make_window(1, pane1));
    app.active_idx = 0;
    (app, w0, w1)
}

fn feed_pane(app: &mut AppState, pid: usize, bytes: &[u8]) {
    let p = crate::tree::find_pane_mut_by_id_global(app, pid).expect("pane exists");
    p.term.lock().expect("parser lock").process(bytes);
}

fn assert_focus_untouched(app: &AppState) {
    assert_eq!(app.active_idx, 0, "a by-id send must not change the active window");
    assert!(app.windows[0].active_path.is_empty(), "a by-id send must not change the active pane");
    assert!(app.windows[1].pane_mru.is_empty(), "a by-id send must not touch the target's MRU list");
}

// ── text ─────────────────────────────────────────────────────────────────────

#[test]
fn text_by_id_lands_in_the_named_pane_only() {
    let (mut app, w0, w1) = app_two_windows();
    send_text_to_pane_by_id(&mut app, 1, "hello").expect("delivery");
    assert_eq!(w1.bytes(), b"hello", "the named pane receives the text");
    assert!(w0.bytes().is_empty(), "the ACTIVE pane must receive nothing — that was the bug");
    assert_focus_untouched(&app);
}

#[test]
fn unknown_pane_id_is_an_error_and_writes_nothing() {
    let (mut app, w0, w1) = app_two_windows();
    let err = send_text_to_pane_by_id(&mut app, 9, "stray").expect_err("no pane %9");
    assert_eq!(err, "can't find pane: %9", "tmux wording, so the client prints what tmux would");
    assert!(w0.bytes().is_empty() && w1.bytes().is_empty(),
        "an unresolvable target must never fall back to the active pane");
    assert_focus_untouched(&app);
}

// ── named keys ───────────────────────────────────────────────────────────────

#[test]
fn named_key_by_id_writes_the_key_sequence_to_the_named_pane() {
    let (mut app, w0, w1) = app_two_windows();
    send_key_to_pane_by_id(&mut app, 1, "enter").expect("delivery");
    assert_eq!(w1.bytes(), b"\r");
    assert!(w0.bytes().is_empty());
    assert_focus_untouched(&app);
}

#[test]
fn cursor_key_by_id_honours_the_named_panes_application_cursor_mode() {
    // DECCKM is per pane: the TARGET's parser decides CSI vs SS3, not the
    // active pane's (tmux parity, MODE_KCURSOR).
    let (mut app, _w0, w1) = app_two_windows();
    feed_pane(&mut app, 1, b"\x1b[?1h");
    send_key_to_pane_by_id(&mut app, 1, "up").expect("delivery");
    assert_eq!(w1.bytes(), b"\x1bOA", "application-cursor pane gets SS3");
}

// ── paste ────────────────────────────────────────────────────────────────────

#[test]
fn paste_by_id_brackets_when_the_named_pane_asked_for_it() {
    let (mut app, w0, w1) = app_two_windows();
    feed_pane(&mut app, 1, b"\x1b[?2004h");
    send_paste_to_pane_by_id(&mut app, 1, "pasted").expect("delivery");
    let got = w1.bytes();
    assert!(got.starts_with(b"\x1b[200~") && got.ends_with(b"\x1b[201~"),
        "bracketed paste markers expected, got {:?}", String::from_utf8_lossy(&got));
    assert!(got.windows(6).any(|w| w == b"pasted"));
    assert!(w0.bytes().is_empty());
    assert_focus_untouched(&app);
}

#[test]
fn paste_by_id_is_plain_when_the_named_pane_did_not_ask() {
    let (mut app, _w0, w1) = app_two_windows();
    send_paste_to_pane_by_id(&mut app, 1, "pasted").expect("delivery");
    assert_eq!(w1.bytes(), b"pasted");
}

// ── raw bytes ────────────────────────────────────────────────────────────────

#[test]
fn bytes_by_id_are_written_verbatim_to_the_named_pane() {
    let (mut app, w0, w1) = app_two_windows();
    send_bytes_to_pane_by_id(&mut app, 1, &[0x1b, 0x5b, 0x41]).expect("delivery");
    assert_eq!(w1.bytes(), b"\x1b[A");
    assert!(w0.bytes().is_empty());
}

// ── the active pane keeps its mode-aware route ───────────────────────────────

#[test]
fn delivery_to_the_active_pane_takes_the_mode_aware_route() {
    // Plain: a by-id send to the active pane reaches its PTY exactly as
    // `send-keys` without -t would.
    let (mut app, w0, _w1) = app_two_windows();
    send_text_to_pane_by_id(&mut app, 0, "typed").expect("delivery");
    assert_eq!(w0.bytes(), b"typed");

    // In copy mode the active route interprets characters as copy-mode
    // motions and writes nothing to the PTY. A by-id send to the active
    // pane must go through that same route — otherwise `send-keys -t %0`
    // would leak keystrokes into a program while the user is copying.
    let (mut app, w0, _w1) = app_two_windows();
    crate::copy_mode::enter_copy_mode(&mut app);
    assert!(matches!(app.mode, Mode::CopyMode), "fixture: copy mode entered");
    send_text_to_pane_by_id(&mut app, 0, "hjkl").expect("delivery");
    assert!(w0.bytes().is_empty(), "copy-mode motions must not reach the PTY");
}

#[test]
fn a_non_active_pane_is_unaffected_by_the_active_panes_copy_mode() {
    // Copy mode belongs to the active pane; pane %1 is not in it, so its
    // keys are ordinary input.
    let (mut app, w0, w1) = app_two_windows();
    crate::copy_mode::enter_copy_mode(&mut app);
    send_text_to_pane_by_id(&mut app, 1, "hjkl").expect("delivery");
    assert_eq!(w1.bytes(), b"hjkl");
    assert!(w0.bytes().is_empty());
    assert!(matches!(app.mode, Mode::CopyMode), "the active pane's copy mode survives");
}

// ── send-keys token delivery through the sink ────────────────────────────────

#[test]
fn deliver_send_keys_by_id_types_tokens_into_the_named_pane() {
    let (mut app, w0, w1) = app_two_windows();
    let keys: Vec<String> = vec!["echo".into(), "hi".into(), "Enter".into()];
    deliver_send_keys(&mut app, &keys, false, SendSink::Pane(1)).expect("delivery");
    // Two plain tokens keep the historical single separator space (#490);
    // the named key follows with no separator.
    assert_eq!(w1.bytes(), b"echo hi\r");
    assert!(w0.bytes().is_empty());
    assert_focus_untouched(&app);
}

#[test]
fn deliver_send_keys_active_sink_is_the_historical_route() {
    let (mut app, w0, w1) = app_two_windows();
    let keys: Vec<String> = vec!["ls".into(), "Enter".into()];
    deliver_send_keys(&mut app, &keys, false, SendSink::Active).expect("delivery");
    assert_eq!(w0.bytes(), b"ls\r");
    assert!(w1.bytes().is_empty());
}

#[test]
fn deliver_send_keys_literal_by_id_types_verbatim() {
    let (mut app, _w0, w1) = app_two_windows();
    let keys: Vec<String> = vec!["Enter".into()];
    deliver_send_keys(&mut app, &keys, true, SendSink::Pane(1)).expect("delivery");
    assert_eq!(w1.bytes(), b"Enter", "-l types the token text, not the key");
}

// ── the routing predicate the connection thread uses ─────────────────────────

#[test]
fn sends_by_pane_id_only_for_send_commands_with_an_id_target() {
    use crate::cli::sends_by_pane_id;
    for cmd in ["send-keys", "send", "send-paste", "send-text", "send-key"] {
        assert!(sends_by_pane_id(cmd, true, Some(3)), "{cmd} -t %3 goes by id");
        assert!(!sends_by_pane_id(cmd, false, Some(3)), "{cmd} -t .3 is an index: focus path");
        assert!(!sends_by_pane_id(cmd, true, None), "{cmd} without a pane: focus path");
    }
    for cmd in ["capture-pane", "kill-pane", "select-pane", "rename-window", "list-panes"] {
        assert!(!sends_by_pane_id(cmd, true, Some(3)), "{cmd} keeps its own resolution");
    }
}
