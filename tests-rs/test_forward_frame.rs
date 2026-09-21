// Framed input for forwarded panes.
//
// The source session used to hand every TCP read chunk of a forwarded
// pane's input straight to the pane writer, so a chunk boundary inside an
// escape sequence or a UTF-8 sequence split it in two writes; the pane
// write queue's coalescing hid that only by luck. Now the target frames each
// write and the source reassembles frames whatever the chunking, so the
// pane writer sees exactly the units the target wrote. These pin the codec,
// the split-sequence case, and the negotiation reply that turns framing on.

use super::*;

fn framed(payloads: &[&[u8]]) -> Vec<u8> {
    let mut w = FrameWriter::new(Vec::new());
    for p in payloads {
        assert_eq!(w.write(p).unwrap(), p.len());
    }
    w.inner
}

fn decode_in_pieces(stream: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut d = FrameDecoder::new();
    let mut out = Vec::new();
    let mut at = 0;
    for &c in cuts {
        out.extend(d.feed(&stream[at..c]).unwrap());
        at = c;
    }
    out.extend(d.feed(&stream[at..]).unwrap());
    assert_eq!(d.buffered(), 0, "everything consumed");
    out
}

#[test]
fn every_write_becomes_one_length_prefixed_frame() {
    let stream = framed(&[b"abc", b"\x1b[A"]);
    assert_eq!(stream, b"\x00\x00\x00\x03abc\x00\x00\x00\x03\x1b[A".to_vec());
}

#[test]
fn empty_writes_send_nothing() {
    let mut w = FrameWriter::new(Vec::new());
    assert_eq!(w.write(b"").unwrap(), 0);
    assert!(w.inner.is_empty());
}

#[test]
fn frames_survive_any_chunking() {
    let paste = "p".repeat(600);
    let units: Vec<&[u8]> = vec![b"\x1b[A", "\u{2014}".as_bytes(), paste.as_bytes(), b"\x1b[200~x\x1b[201~", b"\r"];
    let stream = framed(&units);
    let expect: Vec<Vec<u8>> = units.iter().map(|u| u.to_vec()).collect();
    assert_eq!(decode_in_pieces(&stream, &[]), expect, "one chunk");
    for cut in 1..stream.len() {
        assert_eq!(decode_in_pieces(&stream, &[cut]), expect, "cut at {cut}");
    }
    let every_byte: Vec<usize> = (1..stream.len()).collect();
    assert_eq!(decode_in_pieces(&stream, &every_byte), expect, "one byte at a time");
}

#[test]
fn a_length_prefix_split_across_chunks_is_reassembled() {
    let stream = framed(&[b"hello"]);
    assert_eq!(decode_in_pieces(&stream, &[1, 2, 3]), vec![b"hello".to_vec()]);
}

#[test]
fn a_sequence_split_by_the_stream_reaches_the_pane_writer_whole() {
    // The raw relay would have written "\x1b[2" and "00~x\x1b[201~" as two
    // writes; the pane writer treats each write as whole tokens, so conhost
    // would have seen the first as literal keys.
    let unit = b"\x1b[200~x\x1b[201~";
    let stream = framed(&[unit]);
    let cut = FRAME_HEADER + 3;
    assert_eq!(&stream[FRAME_HEADER..cut], b"\x1b[2", "the chunk boundary lands inside the marker");
    let mut d = FrameDecoder::new();
    assert!(d.feed(&stream[..cut]).unwrap().is_empty(), "nothing delivered until the frame is whole");
    let frames = d.feed(&stream[cut..]).unwrap();
    assert_eq!(frames, vec![unit.to_vec()]);
    assert_eq!(crate::conpty_input::token_len(&frames[0], 0), 6, "the marker is one token again");
    assert_eq!(crate::conpty_input::split_blocks(&frames[0], 256), vec![0..unit.len()]);
}

#[test]
fn an_oversize_length_is_a_protocol_error() {
    let mut d = FrameDecoder::new();
    let mut bad = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
    bad.push(b'x');
    assert!(d.feed(&bad).is_err());
    let mut w = FrameWriter::new(Vec::new());
    assert!(w.write(&vec![0u8; MAX_FRAME + 1]).is_err());
}

#[test]
fn framing_is_enabled_only_by_an_explicit_ok() {
    use crate::proxy_pane::framed_reply_accepted;
    assert!(framed_reply_accepted("OK\n"));
    assert!(framed_reply_accepted("OK"));
    assert!(!framed_reply_accepted(""), "an older source answers nothing");
    assert!(!framed_reply_accepted("ERR\n"), "unknown forward id");
    assert!(!framed_reply_accepted("ERROR: unknown command\n"));
    assert!(!framed_reply_accepted("OK\nrunning\n"), "an OK with a payload is some other command's reply");
}
