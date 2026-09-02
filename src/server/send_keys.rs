//! Delivery of `send-keys` tokens to a pane.
//!
//! The token classifier (named keys, `C-`/`M-`/`C-M-`/`C-S-` chords, F-keys,
//! iTerm2's `0xNN` codepoints, the single separator space between plain
//! tokens) is exactly the one the server loop's `CtrlReq::SendKeys` arm has
//! always had; it moved here unchanged so the same table can feed either the
//! active pane or a pane named by id. The only thing that varies is the sink.
//!
//! Why the sink exists: `send-keys -t %N` used to reach the active-pane arm
//! after a separate temp-focus request, and any other client's request in
//! between moved the focus back — the keys landed in a window the caller
//! never named (2026-09-01, an orchestrator's prompt typed into another
//! agent). `SendSink::Pane` resolves the id inside the request instead.

use std::io;
use std::sync::mpsc;

use crate::input::{
    send_key_to_active, send_key_to_pane_by_id, send_text_to_active, send_text_to_pane_by_id,
};
#[cfg(windows)]
use crate::tree::{active_pane_mut, find_pane_mut_by_id_global};
use crate::tree::get_active_pane_id;
use crate::types::{AppState, CtrlReq, Mode};

/// Where a `send-keys` delivery goes.
pub(crate) enum SendSink {
    /// The active pane of the active window, with every mode (copy, popup,
    /// confirm, menu, synchronized input) honoured — the historical route.
    Active,
    /// The pane with this id, wherever it lives, with no focus change. When
    /// that pane IS the active pane the delivery is delegated to the
    /// `Active` route, so behaviour there is byte-for-byte unchanged.
    Pane(usize),
}

impl SendSink {
    fn text(&self, app: &mut AppState, s: &str) -> io::Result<()> {
        match self {
            SendSink::Active => send_text_to_active(app, s),
            SendSink::Pane(pid) => send_text_to_pane_by_id(app, *pid, s).map_err(not_found),
        }
    }

    fn key(&self, app: &mut AppState, k: &str) -> io::Result<()> {
        match self {
            SendSink::Active => send_key_to_active(app, k),
            SendSink::Pane(pid) => send_key_to_pane_by_id(app, *pid, k).map_err(not_found),
        }
    }

    /// True when the tokens will reach the active pane, i.e. when the
    /// copy-mode routing at the top of `deliver_send_keys` applies. A pane
    /// named by id that is not the active one has no copy mode to route into.
    pub(crate) fn targets_active_pane(&self, app: &AppState) -> bool {
        match self {
            SendSink::Active => true,
            SendSink::Pane(pid) => {
                let Some(win) = app.windows.get(app.active_idx) else { return false };
                win.floating_focus.is_none()
                    && get_active_pane_id(&win.root, &win.active_path) == Some(*pid)
            }
        }
    }

    /// Signal Ctrl+C to the sink pane's console child (SIGINT parity, #338)
    /// before the raw 0x03 byte is written. See the caller for why the order
    /// matters (#579).
    #[cfg(windows)]
    fn ctrl_c_event(&self, app: &mut AppState) {
        let pane = match self {
            SendSink::Active => app
                .windows
                .get_mut(app.active_idx)
                .and_then(|win| active_pane_mut(&mut win.root, &win.active_path)),
            SendSink::Pane(pid) => find_pane_mut_by_id_global(app, *pid),
        };
        if let Some(p) = pane {
            if p.child_pid.is_none() {
                p.child_pid = crate::platform::mouse_inject::get_child_pid(&*p.child);
            }
            if let Some(pid) = p.child_pid {
                crate::platform::mouse_inject::send_ctrl_c_event(pid, false);
            }
        }
    }
}

fn not_found(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, msg)
}

/// Connection-thread half of a send: hand ONE request to the server loop and,
/// for a pane-id target, wait for its verdict. `send` is the channel write;
/// `by_id` builds the `*ToPane` request for `pane_id` with the reply channel;
/// `active` builds the historical fire-and-forget request. A pane-id send
/// blocks until the server has resolved the pane and written the keys (the
/// same blocking contract `capture-pane` has always had), so a client that
/// exits 0 knows the keys reached the pane it named.
pub(crate) fn dispatch_send(
    send: impl Fn(CtrlReq),
    pane_id: Option<usize>,
    by_id: impl FnOnce(usize, mpsc::Sender<Result<(), String>>) -> CtrlReq,
    active: impl FnOnce() -> CtrlReq,
) -> Result<(), String> {
    match pane_id {
        Some(pid) => {
            let (rtx, rrx) = mpsc::channel::<Result<(), String>>();
            send(by_id(pid, rtx));
            match rrx.recv() {
                Ok(verdict) => verdict,
                Err(_) => Err("server did not answer the pane-targeted send".to_string()),
            }
        }
        None => {
            send(active());
            Ok(())
        }
    }
}

/// Deliver `send-keys` operands to `sink`. `keys` are the arguments as
/// SEPARATE tokens (#490): each is either a named key matched in its
/// entirety or literal text typed verbatim, whitespace intact.
pub(crate) fn deliver_send_keys(
    app: &mut AppState,
    keys: &[String],
    literal: bool,
    sink: SendSink,
) -> io::Result<()> {
        let in_copy = sink.targets_active_pane(app)
        && matches!(app.mode, Mode::CopyMode | Mode::CopySearch { .. });
        if in_copy {
            // In copy/search mode — route through mode-aware handlers
            if literal {
                sink.text(app, &keys.join(""))?;
            } else {
                // #490: `keys` holds the send-keys arguments as
                // separate tokens. Match each WHOLE token as a
                // named key or send it verbatim — never split a
                // token on whitespace, which destroyed spacing
                // inside quoted arguments.
                let parts: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
                for key in parts.iter() {
                    let key_upper = key.to_uppercase();
                    let normalized = match key_upper.as_str() {
                        "ENTER" | "RETURN" | "CR" => "enter",
                        "TAB" => "tab",
                        "BTAB" | "BACKTAB" => "btab",
                        "ESCAPE" | "ESC" => "esc",
                        "SPACE" => "space",
                        "BSPACE" | "BACKSPACE" => "backspace",
                        "UP" => "up",
                        "DOWN" => "down",
                        "RIGHT" => "right",
                        "LEFT" => "left",
                        "HOME" => "home",
                        "END" => "end",
                        "PAGEUP" | "PPAGE" => "pageup",
                        "PAGEDOWN" | "NPAGE" => "pagedown",
                        "DELETE" | "DC" => "delete",
                        "INSERT" | "IC" => "insert",
                        _ => "",
                    };
                    if !normalized.is_empty() {
                        sink.key(app, normalized)?;
                    } else if key_upper.starts_with("C-") || key_upper.starts_with("M-") || (key_upper.starts_with("F") && key_upper.len() >= 2 && key_upper[1..].chars().all(|c| c.is_ascii_digit())) {
                        sink.key(app, &key.to_lowercase())?;
                    } else {
                        // Plain text char — route through the sink text path (handles copy mode chars)
                        sink.text(app, key)?;
                    }
                }
            }
        } else if literal {
            sink.text(app, &keys.join(""))?;
        } else {
            // #490: `keys` holds the send-keys arguments as
            // separate tokens. A token either matches a named key
            // in its entirety or is typed verbatim with its
            // whitespace intact; a single separator space is
            // still inserted between adjacent PLAIN tokens for
            // backward compatibility with multi word scripts
            // (strict tmux would concatenate them).
            let parts: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
            for (i, key) in parts.iter().enumerate() {
                let key_upper = key.to_uppercase();
                let _is_special = matches!(key_upper.as_str(),
                    "ENTER" | "RETURN" | "CR" | "TAB" | "BTAB" | "BACKTAB" | "ESCAPE" | "ESC" | "SPACE" | "BSPACE" | "BACKSPACE" |
                    "UP" | "DOWN" | "RIGHT" | "LEFT" | "HOME" | "END" |
                    "PAGEUP" | "PPAGE" | "PAGEDOWN" | "NPAGE" | "DELETE" | "DC" | "INSERT" | "IC" |
                    "F1" | "F2" | "F3" | "F4" | "F5" | "F6" | "F7" | "F8" | "F9" | "F10" | "F11" | "F12"
                ) || key_upper.starts_with("C-") || key_upper.starts_with("M-") || key_upper.starts_with("S-");
                
                match key_upper.as_str() {
                    "ENTER" | "RETURN" | "CR" => sink.text(app, "\r")?,
                    "TAB" => sink.text(app, "\t")?,
                    "BTAB" | "BACKTAB" => sink.text(app, "\x1b[Z")?,
                    "ESCAPE" | "ESC" => sink.text(app, "\x1b")?,
                    "SPACE" => sink.text(app, " ")?,
                    "BSPACE" | "BACKSPACE" => sink.text(app, "\x7f")?,
                    // DECCKM app-cursor mode: SS3, not CSI (see crate::input::csi_cursor_to_ss3).
                    "UP" => sink.key(app, "up")?,
                    "DOWN" => sink.key(app, "down")?,
                    "RIGHT" => sink.key(app, "right")?,
                    "LEFT" => sink.key(app, "left")?,
                    "HOME" => sink.key(app, "home")?,
                    "END" => sink.key(app, "end")?,
                    "PAGEUP" | "PPAGE" => sink.text(app, "\x1b[5~")?,
                    "PAGEDOWN" | "NPAGE" => sink.text(app, "\x1b[6~")?,
                    "DELETE" | "DC" => sink.text(app, "\x1b[3~")?,
                    "INSERT" | "IC" => sink.text(app, "\x1b[2~")?,
                    "F1" => sink.text(app, "\x1bOP")?,
                    "F2" => sink.text(app, "\x1bOQ")?,
                    "F3" => sink.text(app, "\x1bOR")?,
                    "F4" => sink.text(app, "\x1bOS")?,
                    "F5" => sink.text(app, "\x1b[15~")?,
                    "F6" => sink.text(app, "\x1b[17~")?,
                    "F7" => sink.text(app, "\x1b[18~")?,
                    "F8" => sink.text(app, "\x1b[19~")?,
                    "F9" => sink.text(app, "\x1b[20~")?,
                    "F10" => sink.text(app, "\x1b[21~")?,
                    "F11" => sink.text(app, "\x1b[23~")?,
                    "F12" => sink.text(app, "\x1b[24~")?,
                    // Modifier + special key combos (C-Left, S-Right, C-M-Up, etc.)
                    // must be checked BEFORE the generic C-x / M-x single-char handlers.
                    s if crate::input::parse_modified_special_key(s).is_some() => {
                        let seq = crate::input::parse_modified_special_key(s).unwrap();
                        sink.text(app, &seq)?;
                    }
                    s if s.starts_with("C-M-") || s.starts_with("C-m-") => {
                        if let Some(c) = key.chars().nth(4) {
                            if let Some(ctrl) = crate::input::ctrl_char_send_keys_byte(c) {
                                sink.text(app, &format!("\x1b{}", ctrl as char))?;
                            }
                        }
                    }
                    // Ctrl+Shift+<punctuation/digit> that collapses to a single
                    // C0 byte, e.g. Ctrl+/ delivered by ConPTY terminals
                    // (Alacritty, WezTerm) as "C-S--" (VK_OEM_MINUS + Ctrl +
                    // Shift).  It must reach the child as 0x1f (^_), matching
                    // Ctrl+_ and tmux, so neovim's Ctrl+/ comment toggle fires
                    // (issue #394).  This MUST precede the generic C- arm below,
                    // whose nth(2) extraction would otherwise read the 'S' and
                    // mis-send Ctrl+S.
                    s if (s.starts_with("C-S-") || s.starts_with("C-s-"))
                        && s.chars().count() == 5
                        && s.chars().nth(4).map_or(false, |c| !c.is_ascii_alphabetic()) =>
                    {
                        if let Some(c) = s.chars().nth(4) {
                            if let Some(ctrl) = crate::input::ctrl_char_send_keys_byte(c) {
                                sink.text(app, &String::from(ctrl as char))?;
                            }
                        }
                    }
                    s if s.starts_with("C-") => {
                        if let Some(c) = s.chars().nth(2) {
                            let Some(ctrl) = crate::input::ctrl_char_send_keys_byte(c) else { continue };
                            // On Windows with Win32 input mode, write the key as
                            // a Win32 input mode escape sequence so ConPTY generates
                            // a proper KEY_EVENT with VK + LEFT_CTRL_PRESSED (#305).
                            #[cfg(windows)]
                            {
                                if c.is_ascii_alphabetic() {
                                    // Keep Ctrl+C on the legacy interrupt path:
                                    // raw 0x03 + the interrupt router. The router
                                    // runs BEFORE the byte: when it decides "raw
                                    // 0x03 only" it may strip PROCESSED_INPUT from
                                    // the pane console so conhost delivers the byte
                                    // as input instead of converting it into a
                                    // console-wide CTRL_C_EVENT that aborts a
                                    // booting WSL launch (#579).
                                    if ctrl == 0x03 {
                                        sink.ctrl_c_event(app);
                                        sink.text(app, &String::from(ctrl as char))?;
                                    } else {
                                        let vk = crate::platform::mouse_inject::char_to_vk(c);
                                        let scan = crate::platform::mouse_inject::vk_to_scan(vk);
                                        let u_char = (c.to_ascii_lowercase() as u16) & 0x1F;
                                        const LEFT_CTRL_PRESSED: u32 = 0x0008;
                                        let seq = format!(
                                            "\x1b[{};{};{};1;{};1_\x1b[{};{};{};0;{};1_",
                                            vk, scan, u_char, LEFT_CTRL_PRESSED,
                                            vk, scan, u_char, LEFT_CTRL_PRESSED
                                        );
                                        sink.text(app, &seq)?;
                                    }
                                } else {
                                    sink.text(app, &String::from(ctrl as char))?;
                                }
                            }
                            #[cfg(not(windows))]
                            sink.text(app, &String::from(ctrl as char))?;
                        }
                    }
                    s if s.starts_with("M-") => {
                        if let Some(c) = key.chars().nth(2) {
                            sink.text(app, &format!("\x1b{}", c))?;
                        }
                    }
                    _ => {
                        // Plain token: typed VERBATIM (#490 — the
                        // token's own whitespace is untouched).
                        // Keep the historical single separator
                        // space between two adjacent plain tokens
                        // so existing multi word scripts like
                        // `send-keys echo hi Enter` keep working.
                        sink.text(app, key)?;
                        if i + 1 < parts.len() {
                            let next_upper = parts[i + 1].to_uppercase();
                            let next_is_special = matches!(next_upper.as_str(),
                                "ENTER" | "RETURN" | "CR" | "TAB" | "BTAB" | "BACKTAB" | "ESCAPE" | "ESC" | "SPACE" | "BSPACE" | "BACKSPACE" |
                                "UP" | "DOWN" | "RIGHT" | "LEFT" | "HOME" | "END" |
                                "PAGEUP" | "PPAGE" | "PAGEDOWN" | "NPAGE" | "DELETE" | "DC" | "INSERT" | "IC" |
                                "F1" | "F2" | "F3" | "F4" | "F5" | "F6" | "F7" | "F8" | "F9" | "F10" | "F11" | "F12"
                            ) || next_upper.starts_with("C-") || next_upper.starts_with("M-") || next_upper.starts_with("S-");
                            if !next_is_special {
                                sink.text(app, " ")?;
                            }
                        }
                    }
                }
            }
        }
    Ok(())
}
