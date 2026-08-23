# Colour is lost on a cold console attach (crossterm ANSI detection)

Date: 2026-08-17
Affects: psmux 3.3.6 (Windows), every **console** attach path
Severity: cosmetic but confusing — intermittent, sticky for the life of the attach
Fix: ~4 lines in `src/main.rs`

---

## Symptom

Occasionally an attach comes up **entirely monochrome** — both the pane contents *and* psmux's
own status line — while the same session attached from another terminal shows full colour. It is
**intermittent**: detaching and reattaching, or restarting the attaching program, sometimes
restores colour and sometimes does not.

Observed reliably when the attaching terminal is **cold-starting** — Cletus's Live Session Manager
launches a private portable Windows Terminal and fires `tmux attach-session` at it immediately,
which loses colour every so often. A warm terminal attaching by hand essentially never does.

The status line going monochrome alongside the pane is the diagnostic tell: the status line is
drawn by psmux itself through ratatui, so this is not an application inside the pane deciding it
has no colour. One client-level flag turns off both.

## Root cause

Not in psmux's own colour handling — in **crossterm**, which ratatui renders through.

`~/.cargo/registry/src/*/crossterm-0.29.0/src/ansi_support.rs`:

```rust
static SUPPORTS_ANSI_ESCAPE_CODES: AtomicBool = AtomicBool::new(false);
static INITIALIZER: Once = Once::new();

pub fn supports_ansi() -> bool {
    INITIALIZER.call_once(|| {
        let supported = enable_vt_processing().is_ok()
            || std::env::var("TERM").map_or(false, |term| term != "dumb");
        SUPPORTS_ANSI_ESCAPE_CODES.store(supported, Ordering::SeqCst);
    });
    SUPPORTS_ANSI_ESCAPE_CODES.load(Ordering::SeqCst)
}
```

Three properties combine into the bug:

1. **It is decided once and cached for the whole process** (`Once` + `AtomicBool`). Whatever it
   concludes at attach time it keeps until the client exits. There is no re-detection.
2. **`enable_vt_processing()` can fail transiently.** It does
   `Handle::current_out_handle()` → `ConsoleMode::mode()` → `ConsoleMode::set_mode()`. During a
   cold ConPTY/Windows Terminal start the standard output handle is not necessarily a usable
   console handle yet, and any of those three steps can return `Err`.
3. **`TERM` is the fallback that rescues it** — and on Windows `TERM` is usually unset. It is not
   set at User or Machine scope by default, and a launcher chain such as
   scheduled task → app → Windows Terminal → `pwsh -NoProfile` → `tmux attach` carries none.

So on a cold attach: VT enabling fails, `TERM` is absent, `supported = false` is cached, and every
subsequent ratatui draw for that client falls off the ANSI path for the rest of the attach.

`||` short-circuits, so a set `TERM` makes the outcome deterministic regardless of how the
VT-enable race lands.

## Why psmux is *nearly* protected already

`src/main.rs:3959-3973`:

```rust
let mut stdout = crate::platform::create_writer();
enable_virtual_terminal_processing();
if pipe_vt {
    // A Cygwin pty is already raw from the native side (no console line
    // discipline in the path); enable_raw_mode would call SetConsoleMode
    // on the pipe handle and fail with ERROR_INVALID_FUNCTION.
    // crossterm's ANSI detection needs TERM set to take the pure-ANSI
    // path on Windows — mintty always sets it, but make sure.
    if env::var("TERM").is_err() {
        env::set_var("TERM", "xterm-256color");
    }
    let _ = enable_raw_mode();
} else {
    enable_raw_mode()?;
}
```

The `TERM` guard already exists, the comment already names the exact mechanism — *"crossterm's
ANSI detection needs TERM set to take the pure-ANSI path on Windows"* — but it is **scoped to the
`pipe_vt` branch only**. The console branch (`else`) relies entirely on
`enable_virtual_terminal_processing()` at :3960 having succeeded, with no fallback when it did not.

That is the whole gap. The mintty/Cygwin path is covered; the ordinary Windows-console path is not.

Note that psmux's own `enable_virtual_terminal_processing()` at :3960 and crossterm's
`enable_vt_processing()` fail under the same conditions — both go through the same standard output
handle — so psmux's call does not shield crossterm from the race. When one fails the other does too.

## The change

Hoist the `TERM` fallback so it applies to **every** attach path, not just `pipe_vt`:

```rust
let mut stdout = crate::platform::create_writer();
enable_virtual_terminal_processing();

// crossterm decides ANSI support ONCE per process and caches it:
//   enable_vt_processing().is_ok() || TERM != "dumb"
// On a cold ConPTY the first half can fail transiently, and with TERM unset
// (the norm on Windows) the whole client then renders without colour for the
// rest of the attach — pane contents AND our own status line. Setting TERM
// makes the detection deterministic instead of a race. Must happen BEFORE the
// first crossterm call, because the result is latched by a `Once`.
if env::var("TERM").is_err() {
    env::set_var("TERM", "xterm-256color");
}

if pipe_vt {
    // A Cygwin pty is already raw from the native side (no console line
    // discipline in the path); enable_raw_mode would call SetConsoleMode
    // on the pipe handle and fail with ERROR_INVALID_FUNCTION.
    let _ = enable_raw_mode();
} else {
    enable_raw_mode()?;
}
```

**Ordering is load-bearing.** The assignment must precede the first crossterm call that can reach
`supports_ansi()` — `enable_raw_mode()` is the one here. Once the `Once` has fired, setting `TERM`
has no effect for that process.

## Risk

Low, but state it honestly: setting `TERM` makes crossterm report ANSI support **even if VT
processing genuinely could not be enabled**, in which case escape sequences would be written to a
console that does not interpret them and would appear as literal text.

In practice that is not the case being worked around here. ConPTY — which backs Windows Terminal,
VS Code's terminal, and every modern Windows host — always interprets VT. The failure this fixes
is a *transient handle* problem during startup, not a genuinely non-VT console. The legacy
`conhost.exe` without VT is the only real counter-example, and psmux is already unusable there for
other reasons.

`pipe_vt` behaviour is unchanged: it set `TERM` before and still does, just from one line higher.

## Verification

1. Attach from a warm terminal — colour, as before (no regression).
2. Attach from a **cold** terminal, ideally scripted so the attach fires the instant the terminal
   is spawned. Before the change this drops colour every so often; after, it should not.
3. Force the failure path to prove the fallback is what is saving it: temporarily make
   `enable_virtual_terminal_processing()` a no-op and confirm colour still works with the change
   and is lost without it. This is the only test that actually exercises the fix — a normal attach
   succeeds at `enable_vt_processing()` and never reaches the `TERM` half of the `||`.
4. Check `pipe_vt` (mintty/Cygwin) still behaves, since its guard moved.

## Related

The consumer that surfaced this is Cletus's Live Session Manager embedded terminal. It is getting
the same fix independently on its own side — `$env:TERM = 'xterm-256color'` in the generated
`terminal-attach.ps1` wrapper (`Cletus\AgentSessionViewer.fs`, `wrapperScript`) — so that it is
fixed without waiting on a psmux rebuild. The two fixes are complementary, not redundant: the
Cletus one covers that one caller now, this one covers every psmux client.
