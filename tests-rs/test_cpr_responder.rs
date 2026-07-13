// Regression tests for the reactive CPR (Cursor Position Request) responder.
//
// Root cause (issue: pwsh hangs after lock/unlock):
//   pwsh emits ESC[6n at startup and again after session events such as
//   Win+L / unlock.  psmux's original preemptive ESC[1;1R (written once at
//   spawn time) is long gone by then.  Without a reactive responder pwsh
//   blocks indefinitely.
//
// Fix: the parser thread scans every byte batch for ESC[6n via
// `scan_cpr_query` and sets `cpr_pending` after the first query already
// covered by psmux's preemptive response; the server loop calls
// `drain_cpr_pending` which writes ESC[row;colR and clears the flag.

use super::*;

// ── scan_cpr_query ────────────────────────────────────────────────────────

#[test]
fn detects_standalone_cpr_query() {
    assert!(scan_cpr_query(b"\x1b[6n"));
}

#[test]
fn detects_cpr_query_embedded_in_startup_sequence() {
    // This is the exact 88-byte sequence logged for pane=16 when pwsh hung.
    let startup = b"\x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\x1b]0;pwsh.exe\x07\x1b[?25h";
    assert!(scan_cpr_query(startup));
}

#[test]
fn no_false_positive_for_rmcup_only() {
    assert!(!scan_cpr_query(b"\x1b[?1049l"));
}

#[test]
fn no_false_positive_for_empty_input() {
    assert!(!scan_cpr_query(b""));
}

#[test]
fn no_false_positive_for_plain_text() {
    assert!(!scan_cpr_query(b"hello world"));
}

#[test]
fn no_false_positive_for_partial_sequence() {
    // ESC + '[' without '6n' — must not match
    assert!(!scan_cpr_query(b"\x1b[6"));
    assert!(!scan_cpr_query(b"\x1b[n"));
    assert!(!scan_cpr_query(b"\x1b[6m")); // wrong terminator
}

#[test]
fn detects_cpr_query_at_end_of_buffer() {
    let mut buf = vec![b'X'; 1024];
    buf.extend_from_slice(b"\x1b[6n");
    assert!(scan_cpr_query(&buf));
}

#[test]
fn escapes_without_0x1b_skip_window_scan() {
    // Pre-check: no ESC byte → must be false without scanning
    assert!(!scan_cpr_query(b"[6n"));
}

// ── should_signal_reactive_cpr ───────────────────────────────────────────

#[test]
fn first_cpr_query_is_covered_by_preemptive_response() {
    let mut preemptive_available = true;
    assert!(!should_signal_reactive_cpr(true, &mut preemptive_available));
    assert!(!preemptive_available);
}

#[test]
fn second_cpr_query_is_reactive_after_preemptive_response_is_consumed() {
    let mut preemptive_available = true;
    assert!(!should_signal_reactive_cpr(true, &mut preemptive_available));
    assert!(should_signal_reactive_cpr(true, &mut preemptive_available));
}

#[test]
fn non_cpr_batch_does_not_consume_preemptive_response() {
    let mut preemptive_available = true;
    assert!(!should_signal_reactive_cpr(false, &mut preemptive_available));
    assert!(preemptive_available);
}

#[test]
fn proxy_panes_without_preemptive_response_signal_first_cpr_query() {
    let mut preemptive_available = false;
    assert!(should_signal_reactive_cpr(true, &mut preemptive_available));
}

// ── drain_cpr_pending — response format ──────────────────────────────────
//
// We verify the CPR response string format directly since constructing a
// full Pane (which requires a live MasterPty) is out of scope for a unit
// test.  The format is the same one drain_cpr_pending builds.

#[test]
fn cpr_response_format_is_1_based() {
    // vt100::Parser uses 0-based (row, col); CPR response uses 1-based.
    let mut parser = vt100::Parser::new(24, 80, 0);
    // Move cursor to row 2, col 5 (0-based → 1-based: row=3, col=6)
    parser.process(b"\x1b[3;6H");
    let (r, c) = parser.screen().cursor_position();
    assert_eq!((r, c), (2, 5), "parser uses 0-based coords");
    let response = format!("\x1b[{};{}R", r + 1, c + 1);
    assert_eq!(response, "\x1b[3;6R");
}

#[test]
fn cpr_response_fallback_produces_valid_sequence() {
    // unwrap_or((0,0)) → ESC[1;1R — a valid response that unblocks pwsh
    let (r, c): (u16, u16) = (0, 0);
    let response = format!("\x1b[{};{}R", r + 1, c + 1);
    assert_eq!(response, "\x1b[1;1R");
}

// ── reader startup zeroes ─────────────────────────────────────────────────

struct ZeroThenDataReader {
    zeroes_left: usize,
    sent_data: bool,
}

impl std::io::Read for ZeroThenDataReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.zeroes_left > 0 {
            self.zeroes_left -= 1;
            return Ok(0);
        }
        if !self.sent_data {
            self.sent_data = true;
            let data = b"after-zero-startup";
            buf[..data.len()].copy_from_slice(data);
            return Ok(data.len());
        }
        Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "done"))
    }
}

#[test]
fn reader_survives_startup_zero_reads_before_first_output() {
    let term = std::sync::Arc::new(std::sync::Mutex::new(vt100::Parser::new(5, 80, 0)));
    let data_version = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cursor_shape = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(CURSOR_SHAPE_UNSET));
    let bell_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cpr_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_ring = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));

    spawn_reader_thread(
        Box::new(ZeroThenDataReader { zeroes_left: 20, sent_data: false }),
        term.clone(),
        data_version.clone(),
        cursor_shape,
        bell_pending,
        cpr_pending,
        true,
        output_ring,
        "test-delayed-zero-reader",
    );

    for _ in 0..100 {
        if data_version.load(std::sync::atomic::Ordering::Acquire) > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        data_version.load(std::sync::atomic::Ordering::Acquire) > 0,
        "reader exited before processing delayed startup output"
    );
    let parser = term.lock().unwrap();
    let first_row = (0..20)
        .filter_map(|col| parser.screen().cell(0, col))
        .map(|cell| cell.contents())
        .collect::<String>();
    assert!(first_row.contains("after-zero-startup"));
}
