// Regression guard for U+2014 (and friends) vanishing between a psmux send
// and a ConPTY child that reads key-down records only.
//
// The inbox Windows 11 conhost delivers a non-letter, non-wide character the
// keyboard layout cannot produce as an Alt+numpad sequence whose character
// rides only on the Alt key-up record; crossterm-based readers (Codex) drop
// it. A 3536-scalar prompt reached Codex as 3534 with both em dashes gone.
// The pane writer now hands such scalars over as win32-input-mode key-down
// records, which the same conhost turns into the plain vk=0 record a letter
// would have produced. conhost also reads the pipe 256 bytes at a time and
// flushes a sequence cut by a read as plain keys, so the writer paces blocks
// on the pipe's fill level. These pin the encoder (what it rewrites, what it
// leaves alone, a sequence split across writes), the block cutter, and the
// paced writer against a fake pipe.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn enc(input: &[u8]) -> Vec<u8> {
    let mut e = Win32InputEncoder::new();
    let mut out = Vec::new();
    e.encode(input, &mut out);
    assert!(e.pending().is_empty(), "no pending bytes expected for {:?}", input);
    out
}

fn rec(scalar: u32) -> Vec<u8> {
    let mut v = Vec::new();
    push_win32_key_down(&mut v, scalar);
    v
}

// ── encoder ────────────────────────────────────────────────────────────────

#[test]
fn ascii_and_escape_sequences_pass_through_untouched() {
    let input = b"hello\r\x1b[A\x1b[200~x\x1b[201~\x03\x7f";
    assert_eq!(enc(input), input.to_vec());
}

#[test]
fn em_dash_becomes_a_win32_key_down_record() {
    assert_eq!(enc("x\u{2014}y".as_bytes()), [b"x".to_vec(), rec(0x2014), b"y".to_vec()].concat());
    assert_eq!(rec(0x2014), b"\x1b[0;0;8212;1;0;1_".to_vec());
}

#[test]
fn every_scalar_conhost_mangles_is_rewritten() {
    // Measured as Alt+numpad on conhost 10.0.22621.5909: punctuation, symbols,
    // arrows, currency, degree. The letters conhost passes plainly are
    // rewritten too, to the identical record shape, so the rule stays simple.
    for ch in ['\u{2014}', '\u{2013}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
               '\u{2026}', '\u{2022}', '\u{2713}', '\u{2500}', '\u{2192}', '\u{20AC}',
               '\u{00B0}', '\u{00E9}', '\u{00F1}', '\u{03B1}', '\u{4E2D}'] {
        let mut s = String::new();
        s.push(ch);
        assert_eq!(enc(s.as_bytes()), rec(ch as u32), "{:?}", ch);
    }
}

#[test]
fn range_boundaries() {
    assert_eq!(enc("\u{7f}".as_bytes()), b"\x7f".to_vec(), "DEL stays a byte");
    assert_eq!(enc("\u{80}".as_bytes()), rec(0x80), "first non-ASCII scalar");
    assert_eq!(enc("\u{7FFF}".as_bytes()), rec(0x7FFF), "largest expressible Uc");
    assert_eq!(enc("\u{8000}".as_bytes()), "\u{8000}".as_bytes().to_vec(), "beyond the parameter cap: raw");
    assert_eq!(enc("\u{8BED}".as_bytes()), "\u{8BED}".as_bytes().to_vec(), "CJK above the cap: raw");
    assert_eq!(enc("\u{1F600}".as_bytes()), "\u{1F600}".as_bytes().to_vec(), "astral: raw");
}

#[test]
fn a_sequence_split_across_writes_is_held_then_completed() {
    let mut e = Win32InputEncoder::new();
    let mut out = Vec::new();
    e.encode(b"x\xe2\x80", &mut out);
    assert_eq!(out, b"x".to_vec());
    assert_eq!(e.pending(), b"\xe2\x80");
    e.encode(b"\x94y", &mut out);
    assert_eq!(out, [b"x".to_vec(), rec(0x2014), b"y".to_vec()].concat());
    assert!(e.pending().is_empty());
}

#[test]
fn a_four_byte_sequence_split_three_ways_stays_raw_and_intact() {
    let bytes = "\u{1F600}".as_bytes();
    let mut e = Win32InputEncoder::new();
    let mut out = Vec::new();
    e.encode(&bytes[..1], &mut out);
    e.encode(&bytes[1..3], &mut out);
    assert!(out.is_empty());
    e.encode(&bytes[3..], &mut out);
    assert_eq!(out, bytes.to_vec());
    assert!(e.pending().is_empty());
}

#[test]
fn invalid_utf8_passes_through_byte_for_byte() {
    assert_eq!(enc(b"\x80"), b"\x80".to_vec(), "lone continuation byte");
    assert_eq!(enc(b"\xff\xfe"), b"\xff\xfe".to_vec(), "never-valid leads");
    assert_eq!(enc(b"\xe2\x28\xa1"), b"\xe2\x28\xa1".to_vec(), "bad continuation");
    assert_eq!(enc(b"\xe2\x80A"), b"\xe2\x80A".to_vec(), "truncated sequence then ASCII");
    assert_eq!(enc(b"\xc0\xaf"), b"\xc0\xaf".to_vec(), "overlong encoding");
    assert_eq!(enc(b"\xed\xa0\x80"), b"\xed\xa0\x80".to_vec(), "encoded surrogate");
}

#[test]
fn a_bracketed_paste_keeps_its_markers() {
    let input = "\x1b[200~say \u{201C}hi\u{201D} \u{2014} ok\r\x1b[201~";
    let expect = [b"\x1b[200~say ".to_vec(), rec(0x201C), b"hi".to_vec(), rec(0x201D), b" ".to_vec(),
                  rec(0x2014), b" ok\r\x1b[201~".to_vec()].concat();
    assert_eq!(enc(input.as_bytes()), expect);
}

#[test]
fn forced_setting_parses_like_the_mouse_override() {
    assert_eq!(parse_forced_setting(None), None);
    assert_eq!(parse_forced_setting(Some("")), None);
    assert_eq!(parse_forced_setting(Some("maybe")), None);
    for yes in ["1", "on", "TRUE", " yes "] {
        assert_eq!(parse_forced_setting(Some(yes)), Some(true), "{yes}");
    }
    for no in ["0", "off", "False", "no"] {
        assert_eq!(parse_forced_setting(Some(no)), Some(false), "{no}");
    }
}

// ── tokens and blocks ──────────────────────────────────────────────────────

#[test]
fn token_len_covers_every_sequence_shape_psmux_writes() {
    let t = |s: &[u8]| token_len(s, 0);
    assert_eq!(t(b"a"), 1);
    assert_eq!(t(b"\x1b"), 1, "lone ESC");
    assert_eq!(t(b"\x1b[A"), 3, "CSI cursor key");
    assert_eq!(t(b"\x1b[200~x"), 6, "bracketed paste marker");
    assert_eq!(t(b"\x1b[<0;10;20Mrest"), 11, "SGR mouse report");
    assert_eq!(t(b"\x1b[0;0;8212;1;0;1_z"), 17, "win32-input record");
    assert_eq!(t(b"\x1bOA"), 3, "SS3");
    assert_eq!(t(b"\x1bx"), 2, "ESC plus one byte");
    assert_eq!(t(b"\x1b]52;c;aGk=\x07tail"), 12, "OSC ended by BEL");
    assert_eq!(t(b"\x1b]52;c;aGk=\x1b\\tail"), 13, "OSC ended by ST");
    assert_eq!(t(b"\x1b[0;0"), 5, "CSI cut by the buffer end runs to the end");
    assert_eq!(t(b"\x1b[0;\x1b[A"), 4, "an ESC inside a CSI starts a new token");
    assert_eq!(t(b"\x1b]x\x1b[A"), 3, "an ESC inside an OSC starts a new token");
    assert_eq!(t("\u{8BED}z".as_bytes()), 3, "UTF-8 sequence");
    assert_eq!(t(b"\xe8\xaf"), 2, "UTF-8 cut by the buffer end");
    assert_eq!(t(b"\xff"), 1, "invalid lead");
}

#[test]
fn blocks_never_exceed_max_and_never_cut_a_token() {
    let mut buf = Vec::new();
    for _ in 0..60 { push_win32_key_down(&mut buf, 0x2014); }
    let blocks = split_blocks(&buf, 256);
    assert_eq!(blocks.len(), 4, "15 records of 17 bytes fit in 255");
    for r in &blocks {
        assert!(r.len() <= 256);
        assert_eq!(r.len() % 17, 0, "block {:?} cuts a record", r);
    }
    assert_eq!(blocks.last().unwrap().end, buf.len());
}

#[test]
fn a_sequence_that_would_straddle_moves_whole_to_the_next_block() {
    let mut buf = vec![b'a'; 251];
    buf.extend_from_slice(b"\x1b[200~");
    buf.extend_from_slice(b"tail");
    let blocks = split_blocks(&buf, 256);
    assert_eq!(blocks, vec![0..251, 251..261]);
    let mut buf = vec![b'a'; 254];
    buf.extend_from_slice("\u{8BED}".as_bytes());
    assert_eq!(split_blocks(&buf, 256), vec![0..254, 254..257], "UTF-8 sequence too");
}

#[test]
fn plain_text_splits_at_max() {
    let buf = vec![b'a'; 1000];
    assert_eq!(split_blocks(&buf, 256), vec![0..256, 256..512, 512..768, 768..1000]);
    assert!(split_blocks(b"", 256).is_empty());
}

#[test]
fn an_oversize_control_string_is_cut_at_max_like_plain_text() {
    // A 308-byte OSC cannot fit one conhost read, so pacing cannot keep it
    // whole; it is cut at 256 and the remainder tokenized on its own. Every
    // block still respects max, and every byte is still delivered.
    let mut buf = b"pre".to_vec();
    buf.extend_from_slice(b"\x1b]52;c;");
    buf.extend(std::iter::repeat(b'A').take(300));
    buf.extend_from_slice(b"\x07post");
    let blocks = split_blocks(&buf, 256);
    assert_eq!(blocks, vec![0..3, 3..259, 259..315]);
    assert!(blocks.iter().all(|r| r.len() <= 256));
    assert_eq!(blocks.iter().map(|r| r.len()).sum::<usize>(), buf.len());
}

// ── paced writer against a fake pipe ───────────────────────────────────────

/// A pipe stand-in: every write lands as one entry, and the probe reports the
/// bytes written since it was last consulted — one probe call "drains" it,
/// the way conhost's next read would.
#[derive(Clone, Default)]
struct FakePipe {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
    pending: Arc<AtomicUsize>,
    probes: Arc<AtomicUsize>,
}

impl FakePipe {
    fn probe(&self) -> PendingProbe {
        let pending = self.pending.clone();
        let probes = self.probes.clone();
        Box::new(move || {
            probes.fetch_add(1, Ordering::SeqCst);
            Ok(pending.swap(0, Ordering::SeqCst))
        })
    }
    fn writes(&self) -> Vec<Vec<u8>> { self.writes.lock().unwrap().clone() }
}

impl Write for FakePipe {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.writes.lock().unwrap().push(b.to_vec());
        self.pending.fetch_add(b.len(), Ordering::SeqCst);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn sixty_em_dashes_go_out_as_four_whole_record_blocks_each_after_a_drain() {
    let pipe = FakePipe::default();
    let mut w = ConptyInputWriter::new(Some(pipe.probe()), pipe.clone());
    let text = "\u{2014}".repeat(60);
    assert_eq!(w.write(text.as_bytes()).unwrap(), text.len());
    let writes = pipe.writes();
    assert_eq!(writes.len(), 4);
    for b in &writes {
        assert!(b.len() <= 256 && b.len() % 17 == 0, "block of {} bytes", b.len());
    }
    assert_eq!(writes.concat(), enc(text.as_bytes()));
    // First block: probe sees an empty pipe (1 call). Each later block: the
    // previous block is pending (1 call drains it), then empty (1 call).
    assert_eq!(pipe.probes.load(Ordering::SeqCst), 1 + 3 * 2);
}

#[test]
fn plain_text_is_chunked_but_never_waits() {
    let pipe = FakePipe::default();
    let mut w = ConptyInputWriter::new(Some(pipe.probe()), pipe.clone());
    let text = vec![b'a'; 1000];
    w.write(&text).unwrap();
    assert_eq!(pipe.writes().iter().map(Vec::len).collect::<Vec<_>>(), vec![256, 256, 256, 232]);
    assert_eq!(pipe.probes.load(Ordering::SeqCst), 0);
}

#[test]
fn an_escape_block_waits_for_the_text_written_before_it() {
    let pipe = FakePipe::default();
    let mut w = ConptyInputWriter::new(Some(pipe.probe()), pipe.clone());
    w.write(b"typed").unwrap();
    assert_eq!(pipe.probes.load(Ordering::SeqCst), 0);
    w.write(b"\x1b[A").unwrap();
    assert_eq!(pipe.probes.load(Ordering::SeqCst), 2, "saw 'typed' pending, then drained");
    assert_eq!(pipe.writes(), vec![b"typed".to_vec(), b"\x1b[A".to_vec()]);
}

#[test]
fn without_a_probe_the_writer_encodes_but_writes_in_one_piece() {
    let pipe = FakePipe::default();
    let mut w = ConptyInputWriter::new(None, pipe.clone());
    let text = "\u{2014}".repeat(60);
    w.write(text.as_bytes()).unwrap();
    assert_eq!(pipe.writes().len(), 1);
    assert_eq!(pipe.writes()[0], enc(text.as_bytes()));
}

#[test]
fn an_incomplete_tail_is_held_across_a_flush_not_flushed_raw() {
    // write_paste_chunked writes 512-byte chunks and the queue thread flushes
    // after each; a scalar straddling the boundary must still come out whole.
    let pipe = FakePipe::default();
    let mut w = ConptyInputWriter::new(None, pipe.clone());
    let text = format!("{}\u{2014}{}", "a".repeat(511), "b".repeat(10));
    let bytes = text.as_bytes();
    assert_eq!(w.write(&bytes[..512]).unwrap(), 512);
    w.flush().unwrap();
    assert_eq!(pipe.writes().concat().len(), 511, "the lead byte is held, not written");
    assert_eq!(w.write(&bytes[512..]).unwrap(), bytes.len() - 512);
    let expect = [b"a".repeat(511), rec(0x2014), b"b".repeat(10)].concat();
    assert_eq!(pipe.writes().concat(), expect);
}

#[test]
fn wait_drained_gives_up_after_the_cap_when_the_pipe_never_empties() {
    let probe: PendingProbe = Box::new(|| Ok(7));
    let start = std::time::Instant::now();
    assert!(!wait_drained(&probe));
    let waited = start.elapsed();
    assert!(waited >= DRAIN_WAIT_MAX, "{waited:?}");
    assert!(waited < DRAIN_WAIT_MAX * 4, "{waited:?}");
    let broken: PendingProbe = Box::new(|| Err(std::io::Error::other("gone")));
    assert!(!wait_drained(&broken));
}

#[test]
fn a_held_tail_that_turns_out_invalid_goes_out_raw_ahead_of_the_new_data() {
    let mut e = Win32InputEncoder::new();
    let mut out = Vec::new();
    e.encode(b"x\xe2\x80", &mut out);
    assert_eq!(out, b"x".to_vec());
    assert_eq!(e.pending(), b"\xe2\x80");
    e.encode(b"A", &mut out);
    assert_eq!(out, b"x\xe2\x80A".to_vec());
    assert!(e.pending().is_empty());
}

#[test]
fn a_write_ending_inside_an_escape_sequence_is_written_as_is() {
    // A write is whole tokens by contract (local routes write whole units,
    // forwarded input arrives framed), so nothing is held back: a lone ESC at
    // the end of a write is the Escape key and must go out at once.
    let pipe = FakePipe::default();
    let mut w = ConptyInputWriter::new(Some(pipe.probe()), pipe.clone());
    w.write(b"\x1b").unwrap();
    w.write(b"\x1b[").unwrap();
    w.write(b"A").unwrap();
    assert_eq!(pipe.writes(), vec![b"\x1b".to_vec(), b"\x1b[".to_vec(), b"A".to_vec()]);
}

#[test]
fn every_conpty_pane_writer_goes_through_the_async_queue() {
    // The paced writer waits on the pipe (up to DRAIN_WAIT_MAX per escape
    // block); that wait belongs on the pane-writer thread, never on the
    // server loop. Only spawn_conpty_write_queue may wrap a ConPTY writer,
    // and no site may take a ConPTY writer without going through it.
    let pane = include_str!("../src/pane.rs");
    assert_eq!(pane.matches("wrap_pane_writer(").count(), 1, "pane.rs wraps only inside spawn_conpty_write_queue");
    let others = [
        ("popup.rs", include_str!("../src/popup.rs")),
        ("window_ops.rs", include_str!("../src/window_ops.rs")),
        ("cross_session_server.rs", include_str!("../src/cross_session_server.rs")),
        ("server/mod.rs", include_str!("../src/server/mod.rs")),
    ];
    for (name, src) in others {
        assert_eq!(src.matches("wrap_pane_writer(").count(), 0, "{name} must use spawn_conpty_write_queue");
    }
    for (name, src) in [("pane.rs", pane), others[0], others[1]] {
        for line in src.lines() {
            if line.contains("pair.master.take_writer()") {
                assert!(line.contains("spawn_conpty_write_queue("), "{name}: a ConPTY writer taken outside the queue: {line}");
            }
        }
    }
}
