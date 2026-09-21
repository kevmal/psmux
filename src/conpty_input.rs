//! ConPTY input delivery: win32-input-mode encoding for the characters the
//! inbox conhost mangles, and write pacing so conhost never cuts a sequence.
//!
//! # The characters conhost drops
//!
//! conhost translates every UTF-8 byte written to the ConPTY input pipe into
//! KEY_EVENT records for the child (`InteractDispatch::WriteString` →
//! `CharToKeyEvents`).  A character the keyboard layout cannot produce
//! (`VkKeyScanW` fails) is passed as a plain vk=0 key-down only when it is a
//! letter (`C3_ALPHA`) or a wide glyph; everything else — dashes, curly
//! quotes, ellipsis, bullets, arrows, `€`, `°`, and both halves of every
//! surrogate pair — is synthesized as Alt+numpad: Alt down, the digits of the
//! character's best-fit byte in the console output code page, Alt up.  The
//! character itself rides only on the Alt **key-up** record.  crossterm turns
//! that into a `Release` key event, which TUIs built on it (Codex, Helix, ...)
//! discard.  Measured on Windows 11 conhost 10.0.22621.5909: a 3536-scalar
//! prompt pasted into Codex arrived as 3534, both U+2014 gone, while `é` and
//! `中` in the same paste were fine.
//!
//! The same conhost parses win32-input-mode sequences on that pipe
//! (`ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`) into verbatim KEY_EVENT records,
//! so a scalar can be handed over as exactly the plain vk=0 key-down conhost
//! would have produced for a letter.  That is what [`Win32InputEncoder`] does
//! for every scalar in U+0080..=U+7FFF.  conhost caps a CSI parameter at
//! 32767, so scalars above that (most CJK, Hangul, fullwidth forms — wide,
//! hence already passed plainly) and surrogate halves cannot be expressed and
//! stay raw UTF-8.  Bytes below 0x80 — including every escape sequence psmux
//! itself writes — are never rewritten.
//!
//! # The read boundary conhost cuts on
//!
//! conhost's input thread reads the pipe 256 bytes at a time and hands each
//! read to its VT parser as a complete string: a sequence cut by the read
//! boundary is flushed as plain key presses (`ESC`, `[`, `0`, ...), and the
//! rest arrives as more plain keys.  Measured: 60 encoded em dashes in one
//! 1020-byte write reached the child as 56, the records straddling 256, 512,
//! 768 and 1024 spelled out as text.  Every multi-byte VT sequence psmux
//! writes (arrow keys, mouse reports, bracketed-paste markers) has always
//! been exposed to the same cut; it just rarely straddled.
//!
//! [`ConptyInputWriter`] therefore writes in blocks of at most 256 bytes
//! that end only between tokens (a whole escape sequence, a whole UTF-8
//! sequence, or a byte), and before a block that carries an escape it waits
//! until conhost has drained what was written before, so the next read
//! starts exactly at the block.  The drain probe asks the pipe's write end
//! how many written bytes are still unread (`MasterPty::try_clone_input_pending`);
//! it must not touch the read end, whose file object conhost's blocking
//! ReadFile holds.
//!
//! The writer sits inside the pane writer (`pane::spawn_pane_write_queue`),
//! so every route into a pane gets both: `send-keys`, `send-paste`,
//! `send-text`, keys typed at an attached client, and the bracketed-paste
//! chunks of `write_paste_chunked`.  A multi-byte sequence split across two
//! writes is held until its tail arrives.

use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Environment override: `1`/`on` forces the encoding and pacing on any
/// build, `0`/`off` pins them off.  Unset keeps the build check.
pub const WIN32_INPUT_ENV: &str = "PSMUX_CONPTY_WIN32_INPUT";

/// Minimum Windows build whose inbox conhost parses win32-input-mode
/// sequences on the ConPTY input pipe.  Windows 11 conhost does; Windows 10
/// builds were not measured, so they keep the raw UTF-8 path.
pub const CONPTY_WIN32_INPUT_MIN_BUILD: u32 = 22000;

/// Largest scalar a win32-input-mode `Uc` parameter can carry: conhost clamps
/// every CSI parameter to 32767 (a `Uc` of 55357 arrives as U+7FFF).
pub const WIN32_INPUT_MAX_SCALAR: u32 = 0x7FFF;

/// Bytes conhost's input thread takes per read of the ConPTY input pipe.
pub const CONHOST_INPUT_READ: usize = 256;

/// Longest a block write waits for conhost to drain the pipe before going
/// ahead anyway.  conhost normally drains a read in microseconds; the cap
/// only matters if its input thread is starved or gone.
pub const DRAIN_WAIT_MAX: Duration = Duration::from_millis(250);

/// Bytes the child side has not read from the input pipe yet.
pub type PendingProbe = Box<dyn Fn() -> std::io::Result<usize> + Send>;

/// Parses the override value into an explicit yes/no.  Unset, empty, or
/// unrecognised values yield `None`, meaning "fall back to the build check".
pub fn parse_forced_setting(raw: Option<&str>) -> Option<bool> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Whether pane writers on this host encode U+0080..=U+7FFF as
/// win32-input-mode records and pace their writes.  Evaluated once per
/// server process.
pub fn win32_input_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        if let Some(forced) = parse_forced_setting(std::env::var(WIN32_INPUT_ENV).ok().as_deref()) {
            return forced;
        }
        if !cfg!(windows) {
            return false;
        }
        crate::ssh_input::windows_build_number()
            .map_or(false, |b| b >= CONPTY_WIN32_INPUT_MIN_BUILD)
    })
}

/// Appends the win32-input-mode key-down record for `scalar` to `out`:
/// vk 0, scan code 0, the scalar as `Uc`, key down, no modifiers, repeat 1 —
/// the record shape conhost itself emits for a VT character it can pass.
pub fn push_win32_key_down(out: &mut Vec<u8>, scalar: u32) {
    let _ = write!(out, "\x1b[0;0;{};1;0;1_", scalar);
}

fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// Stateful byte encoder.  ASCII and invalid UTF-8 pass through unchanged;
/// scalars in U+0080..=U+7FFF become win32-input-mode records; scalars above
/// that keep their UTF-8 bytes.  An incomplete trailing sequence is kept until
/// the next call completes it.
#[derive(Default)]
pub struct Win32InputEncoder {
    pending: Vec<u8>,
}

impl Win32InputEncoder {
    pub fn new() -> Self { Self::default() }

    /// Bytes held back because they end mid-sequence.
    pub fn pending(&self) -> &[u8] { &self.pending }

    pub fn encode(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let joined: Vec<u8>;
        let buf: &[u8] = if self.pending.is_empty() {
            input
        } else {
            let mut j = std::mem::take(&mut self.pending);
            j.extend_from_slice(input);
            joined = j;
            &joined
        };
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            if b < 0x80 {
                out.push(b);
                i += 1;
                continue;
            }
            let Some(need) = utf8_len(b) else {
                out.push(b);
                i += 1;
                continue;
            };
            if i + need > buf.len() {
                let tail = &buf[i..];
                if tail[1..].iter().all(|c| (0x80..=0xBF).contains(c)) {
                    self.pending = tail.to_vec();
                    return;
                }
                out.push(b);
                i += 1;
                continue;
            }
            match std::str::from_utf8(&buf[i..i + need]) {
                Ok(s) => {
                    let scalar = s.chars().next().map_or(0, |c| c as u32);
                    if (0x80..=WIN32_INPUT_MAX_SCALAR).contains(&scalar) {
                        push_win32_key_down(out, scalar);
                    } else {
                        out.extend_from_slice(&buf[i..i + need]);
                    }
                    i += need;
                }
                Err(_) => {
                    out.push(b);
                    i += 1;
                }
            }
        }
    }
}

/// Length of the token that starts at `buf[i]`: a whole escape sequence (CSI
/// through its final byte, a string sequence through BEL or ST, SS3 plus its
/// byte, or ESC plus one byte), a whole UTF-8 sequence, or one byte.  A
/// sequence the buffer ends inside runs to the end of the buffer; an ESC met
/// inside another sequence starts a new token.
pub fn token_len(buf: &[u8], i: usize) -> usize {
    let rest = buf.len() - i;
    let b = buf[i];
    if b == 0x1b {
        let Some(&next) = buf.get(i + 1) else { return 1 };
        return match next {
            b'[' => {
                let mut j = i + 2;
                while j < buf.len() {
                    match buf[j] {
                        0x40..=0x7E => return j + 1 - i,
                        0x1b => return j - i,
                        _ => j += 1,
                    }
                }
                rest
            }
            b']' | b'P' | b'^' | b'_' | b'X' => {
                let mut j = i + 2;
                while j < buf.len() {
                    match buf[j] {
                        0x07 => return j + 1 - i,
                        0x1b if buf.get(j + 1) == Some(&b'\\') => return j + 2 - i,
                        0x1b => return j - i,
                        _ => j += 1,
                    }
                }
                rest
            }
            b'O' => rest.min(3),
            _ => 2,
        };
    }
    if b >= 0x80 {
        if let Some(n) = utf8_len(b) {
            return n.min(rest);
        }
    }
    1
}

/// Cuts `buf` into blocks of at most `max` bytes whose boundaries fall only
/// between tokens.  A single token longer than `max` is its own block.
pub fn split_blocks(buf: &[u8], max: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < buf.len() {
        let n = token_len(buf, i);
        if i > start && i + n - start > max {
            out.push(start..i);
            start = i;
        }
        i += n;
    }
    if start < buf.len() {
        out.push(start..buf.len());
    }
    out
}

/// Blocks until `probe` reports an empty pipe, an error, or
/// [`DRAIN_WAIT_MAX`] has passed.  Returns whether the pipe was seen empty.
pub fn wait_drained(probe: &PendingProbe) -> bool {
    let start = Instant::now();
    let mut spins = 0u32;
    loop {
        match probe() {
            Ok(0) => return true,
            Err(_) => return false,
            Ok(_) => {}
        }
        if start.elapsed() > DRAIN_WAIT_MAX {
            return false;
        }
        if spins < 500 {
            spins += 1;
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A pane writer that encodes every write through a [`Win32InputEncoder`]
/// and, given a probe, writes it in conhost-read-sized blocks that never cut
/// a token, each escape-bearing block only once the pipe has drained.
pub struct ConptyInputWriter<W: Write> {
    inner: W,
    encoder: Win32InputEncoder,
    probe: Option<PendingProbe>,
    scratch: Vec<u8>,
}

impl<W: Write> ConptyInputWriter<W> {
    pub fn new(probe: Option<PendingProbe>, inner: W) -> Self {
        Self { inner, encoder: Win32InputEncoder::new(), probe, scratch: Vec::new() }
    }
}

impl<W: Write> Write for ConptyInputWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.scratch.clear();
        self.encoder.encode(buf, &mut self.scratch);
        match &self.probe {
            None => self.inner.write_all(&self.scratch)?,
            Some(probe) => {
                for range in split_blocks(&self.scratch, CONHOST_INPUT_READ) {
                    let block = &self.scratch[range];
                    if block.contains(&0x1b) {
                        wait_drained(probe);
                    }
                    self.inner.write_all(block)?;
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}

/// Wraps a ConPTY input writer when this host needs the encoding; otherwise
/// hands the writer back untouched.
pub fn wrap_pane_writer(probe: Option<PendingProbe>, inner: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
    if win32_input_enabled() {
        Box::new(ConptyInputWriter::new(probe, inner))
    } else {
        inner
    }
}

#[cfg(test)]
#[path = "../tests-rs/test_conpty_input.rs"]
mod tests_conpty_input;
