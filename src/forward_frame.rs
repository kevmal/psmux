//! Framing for cross-session pane input.
//!
//! A forwarded pane's input travels target → source over a plain TCP stream.
//! The source used to hand every `read` chunk straight to the real pane's
//! writer, so a chunk boundary could fall inside an escape sequence or a
//! UTF-8 sequence, and the ConPTY writer (which treats each write as whole
//! tokens) would pass the two halves through separately — conhost then
//! flushes the first half as literal keys.  The pane write queue's
//! coalescing hid that most of the time, which is a race, not a guarantee.
//!
//! With framing, the target prefixes every write with its length and the
//! source hands each frame to the pane writer whole, so the units the target
//! wrote (a key, a `send-keys` string, a paste chunk) are the units the pane
//! writer sees, however TCP splits them.  Framing is negotiated: the target
//! sends `pane-forward-framed <id>` on the control connection before it
//! connects the I/O stream, and frames only if the source answers `OK`.  A
//! source that does not know the command answers nothing, so an older source
//! keeps receiving the raw stream, and an older target never asks.

use std::io::{self, Write};

/// Bytes of the big-endian length prefix before every frame.
pub const FRAME_HEADER: usize = 4;

/// Largest frame the decoder accepts.  Anything bigger is a protocol error
/// (the target never writes more than a paste chunk per frame; the pane
/// write queue may coalesce a few of those).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Wraps a stream writer so every `write` goes out as one length-prefixed
/// frame.  Empty writes send nothing.
pub struct FrameWriter<W: Write> {
    inner: W,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self { Self { inner } }
}

impl<W: Write> Write for FrameWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
        }
        let mut out = Vec::with_capacity(FRAME_HEADER + buf.len());
        out.extend_from_slice(&(buf.len() as u32).to_be_bytes());
        out.extend_from_slice(buf);
        self.inner.write_all(&out)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }
}

/// Reassembles frames from arbitrary stream chunks.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self { Self::default() }

    /// Appends a chunk and returns every frame it completes, in order.
    /// Bytes of a frame still in flight stay buffered for the next chunk.
    /// A length above [`MAX_FRAME`] is a protocol error: the decoder returns
    /// it and must not be fed again.
    pub fn feed(&mut self, chunk: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        let mut at = 0;
        while self.buf.len() - at >= FRAME_HEADER {
            let len = u32::from_be_bytes([self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]]) as usize;
            if len > MAX_FRAME {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes exceeds {MAX_FRAME}")));
            }
            if self.buf.len() - at - FRAME_HEADER < len {
                break;
            }
            let start = at + FRAME_HEADER;
            frames.push(self.buf[start..start + len].to_vec());
            at = start + len;
        }
        self.buf.drain(..at);
        Ok(frames)
    }

    /// Bytes buffered for a frame not yet complete.
    pub fn buffered(&self) -> usize { self.buf.len() }
}

#[cfg(test)]
#[path = "../tests-rs/test_forward_frame.rs"]
mod tests_forward_frame;
