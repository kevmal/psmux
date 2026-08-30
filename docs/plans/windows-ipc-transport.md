# Windows IPC transport: should psmux stop paying an ephemeral port per command?

**Status:** proposal, for decision. No code changes accompany this document.
**Date:** 2026-08-30
**Scope:** the client↔server transport only. The callers that generate the
traffic, and the Cletus bug that escalates a failed CLI call into a deleted
session, are being fixed separately and are out of scope here.

---

## 1. The decision in one page

psmux uses **TCP over loopback**, with **one server process per session** and a
**new connection per one-shot CLI command**. The client half-closes its write
side and reads the reply to EOF. That half-close makes the *client* the active
closer of every connection, so every `tmux capture-pane` / `tmux list-panes`
parks one **client-side ephemeral port in TIME_WAIT** for the Windows
`TcpTimedWaitDelay` (2 minutes by default).

Measured on this machine on 2026-08-30 (given, not re-derived here):

| Quantity | Value |
|---|---|
| `tmux.exe` client spawns | ~40/sec sustained |
| New TCP connections to psmux servers | ~90/sec |
| Sockets in TIME_WAIT | 14,300 |
| Windows dynamic port range | 16,384 (49152–65535) |
| Ephemeral port utilisation | **87%** |
| Symptom | `psmux: Only one usage of each socket address ...` (WSAEADDRINUSE / 10048), exit 1; 1–2 of 11 `list-panes` failed on every sweep |

The ~90 connections against ~40 spawns is consistent with the code: the common
CLI paths perform a **liveness probe connection and then a second connection for
the command itself** (`probe_session_alive_inner` in `src/main.rs:424` opens its
own socket before `send_control_with_response` in `src/session.rs:1946` opens
another). *This 2.25× ratio is inference from reading the call paths, not a
measurement.*

**Recommendation, in short:** yes, psmux should stop paying an ephemeral port per
command — but the durable fix is a **Windows named-pipe transport (Option 2)**,
not AF_UNIX, and not connection pooling. Ship it behind a **dual-listen
migration** so live sessions are never broken. If something is needed sooner
than that lands, **Option 6** (stop the client-side half-close so the *server*
becomes the active closer) is a small, correct change that removes the
client-side TIME_WAIT accumulation on its own. Full reasoning in §5–§7.

---

## 2. Where the transport actually is

This was read out of the source; every claim below has a file and line.

### 2.1 There is no async runtime

`Cargo.toml` dependencies are: `ratatui`, `crossterm`, `portable-pty` (vendored),
`which`, `chrono`, `vt100` (vendored), `unicode-width`, `serde`, `serde_json`,
`regex`, `glob`, `anyhow`, `base64`, and `windows-sys` (features
`Win32_Foundation`, `Win32_System_Memory`, `Win32_System_DataExchange`,
`Win32_Storage_FileSystem`).

**There is no `tokio`, no `mio`, no `async-std`, and no futures executor.** The
server is `std::net` + `std::thread`, blocking I/O, **thread per connection**:

```rust
// src/server/mod.rs:1024
thread::spawn(move || {
    connection::handle_connection(stream, tx, &session_key_clone, aliases);
});
```

This single fact reshapes the options. Every "the idiomatic answer is tokio's
`NamedPipeServer`" recommendation is inapplicable without first adopting an async
runtime, which would be a far larger change than the transport swap itself.
Conversely, it makes the *blocking* Win32 named-pipe API a natural fit, and it
makes `uds_windows` (blocking, std-shaped) a natural fit — the async-readiness
question that usually dominates this comparison is simply moot here.

### 2.2 The listener

```rust
// src/server/mod.rs:908
let listener = TcpListener::bind(("127.0.0.1", 0))?;
let port = listener.local_addr()?.port();
```

Bound *before* config load, deliberately, so `run-shell` commands spawned during
config parsing can connect back (comment at `src/server/mod.rs:902`).

A second, separate listener exists for cross-session pane transfer:
`TcpListener::bind("127.0.0.1:0")` at `src/cross_session_server.rs:98`, which
accepts exactly one connection to tunnel PTY I/O. It is low-volume and not part
of the churn, but it is part of any transport swap.

### 2.3 Address discovery — the `.port` / `.key` handshake

There is no registry and no socket path. Discovery is **two files in the data
directory**:

| File | Written by | Contents |
|---|---|---|
| `~/.psmux/<base>.port` | `src/server/mod.rs`, `ensure_session_registry_files` | the TCP port, as ASCII |
| `~/.psmux/<base>.key` | `src/server/mod.rs:945` | a 16-hex-char shared secret |
| `~/.psmux/<base>.sid` | same | stable session id |
| `~/.psmux/<base>.pid` | same | `pid` or `pid:creation_filetime` |

Paths come from `src/paths.rs`: `port_file()` (`:145`), `key_file()` (`:176`),
`sid_file()` (`:181`), all rooted at `psmux_dir()` (`:102`), which honours
`PSMUX_DATA_DIR`. `<base>` is `{socket_name}__{session_name}` under `-L`, else
the bare session name.

A client therefore: reads `.port` → parses a `u16` → `connect_timeout` to
`127.0.0.1:<port>` → sends `AUTH <key>\n` → optionally `TARGET <spec>\n` → the
command → reads the reply.

`.port` is **not** an internal detail. It is documented as a user-facing
diagnostic in `docs/diagnostics.md:121`, and **446 files under `tests/` read it
directly**. That is a hard constraint on migration (§6).

### 2.4 The wire protocol and its framing

The server's `handle_connection` (`src/server/connection.rs:351`) reads an
`AUTH` line, answers `OK\n`, then branches three ways:

- **`PERSISTENT`** (`:430`) — the attach path. Registers a frame channel, a
  directive channel and a client-registry entry, and spawns a writer thread.
  This is a *TUI client*, and it shows up in `list-clients`.
- **`CONTROL` / `CONTROL_NOECHO`** (`src/main.rs:4938`, `run_control_mode`) — tmux
  control mode. **Properly framed:** every command's reply is wrapped in
  `%begin <ts> <n> <flags>` … `%end <ts> <n> <flags>` / `%error`
  (`src/server/connection.rs:857`).
- **one-shot** (everything else) — the hot path. After the command, the server
  sets a **10 ms** read timeout (`src/server/connection.rs:926`) to drain any
  batched follow-up commands, then breaks out of the loop, returns, drops the
  stream, and the socket closes.

The one-shot reply has **no length prefix and no terminator**. The client knows
the reply is complete because **the connection closes**. Both ends of that
contract are explicit in the source:

```rust
// src/session.rs:1976 (send_control_with_response)
// Half-close so the server sees EOF after our request and closes the socket
// once the reply is complete — giving a definitive Ok(0) end-of-response
// instead of relying on an idle-gap timeout to guess the reply is done.
let _ = stream.shutdown(std::net::Shutdown::Write);
```

and the read loop below it breaks on `Ok(0)`. `src/cross_session.rs:35`
(`send_to_session`) does the same read-to-EOF.

**This close-as-end-of-response framing is the structural obstacle to every
form of connection reuse.** It is not incidental; §3 explains why it is there.

### 2.5 The call sites that would change

There is **no single `connect()` chokepoint.** `AUTH {}` is written from **22
distinct sites**:

| File | Lines |
|---|---|
| `src/session.rs` | 1376, 1526, 1548, 1583, 1792, 1912, 1971, 2040 |
| `src/main.rs` | 438, 919, 1044, 2287, 2405, 2716, 4628, 5086 |
| `src/client.rs` | 1458, 3598 |
| `src/proxy_pane.rs` | 65, 139, 213 |
| `src/cross_session.rs` | 43 |

The nearest thing to an abstraction is in `src/session.rs`:

- `open_authed()` (`:1571`) — "Centralizes: CRLF/NUL key validation, connect
  timeout, read timeout, TCP_NODELAY, response size cap, the AUTH + command
  write." Returns a `BufReader<Take<TcpStream>>` — **the concrete type is in the
  signature.**
- `send_auth_cmd()` (`:1518`), `send_auth_cmd_response()` (`:1540`),
  `fetch_authed_response()` (`:1652`), `fetch_authed_response_multi()` (`:1669`),
  `fetch_session_info()` (`:1684`), `send_control()` (`:1888`),
  `send_control_with_response()` (`:1946`), `send_control_to_port()` (`:2036`).

`handle_connection(stream: TcpStream, …)` and `Connection = (TcpStream, Receiver<String>)`
(`src/client.rs:1530`) likewise name `TcpStream` concretely, as do
`ProxyPane`'s `reader_stream`/`writer_stream` fields (`src/proxy_pane.rs:26-28`).

**Any transport change is a type-level change across all of these.** The honest
first step for Options 1 and 2 alike is a `trait ControlStream` (or a
`enum Transport { Tcp(TcpStream), Pipe(PipeStream) }`) plus a
`connect_control(base) -> io::Result<Transport>` chokepoint — a mechanical but
wide refactor that is worth doing *before* and *independently of* choosing a
transport, because it is the part that makes the rest reviewable.

### 2.6 Two things the current transport also costs us

Worth weighing, because they are paid for by TCP specifically:

**(a) Ports are anonymous and recyclable, so identity needs a protocol probe.**
`src/session.rs` carries a whole `AuthProbe` machine
(`Authenticated` / `Rejected` / `Unknown`, `:1362-1396`) whose `Rejected`
variant is documented as "a *different* psmux server has reused this port; the
session this file names is dead." The same hazard is called out at
`src/server/mod.rs:290` and `src/main.rs:1485`. A **named** endpoint cannot be
recycled onto a stranger, so this class disappears with the transport.

*Inference, flagged as such:* at 87% ephemeral utilisation the OS is recycling
ports far more aggressively than usual, which should *raise* the rate of stale
`.port` files pointing at a live-but-unrelated socket. If session-identity
weirdness has been observed during this incident, that is a plausible mechanism.
I did not attempt to confirm it.

**(b) Authentication is a shared secret in a file, not an OS-enforced identity.**
The `.key` file's protection is the profile directory's ACL. A named pipe can be
created with an explicit DACL and the server can call
`GetNamedPipeClientProcessId`, which is a materially stronger story. Not a
reason to migrate on its own; a real bonus if we migrate anyway.

---

## 3. Why TIME_WAIT lands on the client — and why that is our own fix biting

TCP puts the **active closer** (the side that sends the first FIN) into
TIME_WAIT for 2×MSL. In `send_control_with_response` the client calls
`shutdown(Shutdown::Write)` *before* reading the reply, so the client sends FIN
first. It then reads the reply, receives the server's FIN, ACKs it, and enters
**TIME_WAIT holding its ephemeral port**. Multiply by ~90/sec against a 2-minute
retention and you get ~10,800 sockets parked steady-state — the same order as the
14,300 measured.

That half-close was added deliberately, in **PR #464, "fix(ipc): make one-shot
CLI commands reliable"** (merged 2026-07-11). Its description is the clearest
statement of design intent that exists in this repo's history:

> **`kill-session` no-op with exit 0.** `send_control` is fire-and-forget with no
> ack — the client closes the socket ~immediately after writing, so on Windows
> loopback an unread-data RST can make the server drop the command before it is
> dispatched.
>
> **Fix (client-side; no wire-protocol change)** — `send_control`: **half-close**
> the write side after sending so the server sees EOF *after* our bytes.

So: the half-close exists to stop Windows loopback RSTs from silently discarding
commands, and it was explicitly chosen *because it required no wire-protocol
change*. It fixed a real correctness bug. It also, as a side effect, moved the
active close — and therefore TIME_WAIT — onto the client's ephemeral port. **The
current exhaustion is the delayed cost of that trade.** Any option that removes
the half-close must supply another answer to the RST problem, or it will
reintroduce #464.

### Upstream design intent on the transport itself

**There is none that I could find, and I looked.** Searches of `psmux/psmux`
issues and PRs for *named pipe*, *unix socket*, *AF_UNIX*, *TIME_WAIT*, *port
exhaustion*, *10048*, *address in use*, *loopback TCP*, and *transport* returned
no matching design discussion. There is no `TODO`/`FIXME`/`HACK` comment
anywhere in `src/session.rs`, `src/server/mod.rs`, `src/server/connection.rs`,
`src/cross_session*.rs`, or `src/platform.rs` about the transport. No AF_UNIX or
named-pipe code or comment exists in the tree (the sole `pipe` hit in
`src/platform.rs:4562` is about Cygwin ptys).

The closest adjacent artefacts are:

- **PR #464** (above) — the one-shot IPC reliability pass, which chose to keep
  the wire protocol.
- **`tests/bench/bench_ipc_compare.ps1`** — an authoring benchmark that measures
  "PART 1A: CLI latency — cold-process `display-message` round-trip" against
  "PART 1B: RAW TCP round-trip over ONE persistent socket". Someone has already
  quantified the per-command process+connection overhead versus a reused socket.
  It is a latency benchmark, not a transport proposal.

Treat the transport as **undesigned territory upstream**, not as a settled
decision we would be reversing.

---

## 4. What the benchmark reveals about reuse (important for Option 3)

`bench_ipc_compare.ps1` opens one socket, sends `AUTH`, sends `PERSISTENT`, then
loops 100× sending `list-sessions\n` and calling `ReadLine()`. So a persistent,
multiplexed request/response mode **already exists and already works**.

Two caveats stop this from being a free win:

1. **`ReadLine()` is why it works.** The benchmark reads exactly one line and
   does not care about the rest. `PERSISTENT` mode has **no reply terminator**,
   so it cannot frame a multi-line reply — and `capture-pane`, `list-panes`,
   `show-buffer` are all multi-line with arbitrary content. This is precisely why
   the one-shot path uses close-as-EOF instead.
2. **`PERSISTENT` registers a client.** It allocates a frame channel, a directive
   channel and a `client_registry` entry (`src/server/connection.rs:430-480`).
   Routing it through every `capture-pane` would create a phantom attached client
   per CLI call, visible to `list-clients` and to active-window semantics.

The mode that *does* frame multi-line replies correctly is **control mode**
(`%begin`/`%end`/`%error`, `src/server/connection.rs:857`). It also registers a
client, but it proves the framing work is already done once in this codebase and
would not have to be invented.

---

## 5. The options

Effort is in engineer-days for someone fluent in this codebase, and assumes the
`trait ControlStream` refactor from §2.5 is done first (**~2–3 days on its own**,
counted once, not per option).

### Option 1 — AF_UNIX on Windows

**How it works.** Windows has supported `AF_UNIX` `SOCK_STREAM` since Windows 10
1803 / build 17063 (GA in 1809). The server binds a socket file next to the
`.port` file; clients connect by path. No ports, so no ephemeral pool, no
TIME_WAIT on an ephemeral port, and no port-reuse identity confusion.

**Real state of Rust support — this is the part that decides it.**

- **`std` does not support it.** `std::os::unix::net` is Unix-only. The attempt to
  add `std::os::windows::net::{UnixStream, UnixListener}` is
  [rust-lang/rust#147335](https://github.com/rust-lang/rust/pull/147335), gated on
  the unstable feature `windows_unix_domain_sockets` (tracking issue #56533). **It
  was closed in November 2025 over an unresolved licensing conflict** — the
  implementation was adapted from MIT-only `mio-uds-windows` and std requires dual
  MIT/Apache-2.0. So this is not merely "nightly-only"; the merge path is
  currently blocked on a legal question, not an engineering one.
- **So a third-party crate is required: [`uds_windows`](https://lib.rs/crates/uds_windows).**
  v1.2.1, released 2026-03-14; ~4.9M downloads/month; 2,106 dependents. Healthy
  and widely relied on.
- **Async fit is a non-question** — psmux has no async runtime (§2.1), and
  `uds_windows` is blocking and std-shaped, so it drops into the existing
  thread-per-connection model directly.
- **Known gaps that matter to *this* codebase:**
  - **No `connect_timeout()`.** `uds_windows::UnixStream` has `connect()`,
    `try_clone()`, `shutdown()`, `set_read_timeout()`, `set_write_timeout()`,
    `set_nonblocking()`, `local_addr()`, `peer_addr()`, `pair()` — but no
    `connect_timeout`. psmux calls `TcpStream::connect_timeout` at essentially
    every one-shot site, and PR #464 added those timeouts specifically to kill the
    21-second SYN-retransmit hang that surfaced as `os error 10060`. We would have
    to rebuild that guard from `set_nonblocking` + poll, or a thread-with-deadline.
    *Mitigating, and flagged as inference:* on AF_UNIX a connect to a missing path
    fails immediately rather than retransmitting, so the specific 21 s pathology
    largely evaporates; a full listen backlog can still block.
  - No `SOCK_DGRAM`, no fd passing. **psmux uses neither**, so these commonly-cited
    gaps are irrelevant here.
  - `try_clone()` and `shutdown()` are both present — so the half-close framing
    and the `write_stream = stream.try_clone()` pattern survive intact. That is a
    genuine advantage over Option 2.
- Socket files need unlink-on-exit handling and stale-file cleanup, which is a new
  failure mode adjacent to the stale-`.port` logic we already maintain.

**Code changes.** The §2.5 refactor, plus a `uds_windows` dependency, plus
reimplementing `connect_timeout`, plus socket-file lifecycle. `cross_session_server.rs`
and `proxy_pane.rs` come along.

**Effort:** ~4–6 days after the refactor.
**Risk:** medium. New third-party dependency in the critical path; a hand-rolled
`connect_timeout` replacement is exactly the kind of code that regresses #464.
**Ships without breaking live sessions?** Yes, with dual-listen (§6).

### Option 2 — Windows named pipes — **recommended**

**How it works.** The server creates `\\.\pipe\psmux-<data_root_tag>-<base>` with
`PIPE_UNLIMITED_INSTANCES`, and an accept thread loops
`CreateNamedPipeW` → `ConnectNamedPipe` → hand the instance to a worker thread →
create the next instance. Clients `CreateFileW` the same name;
`WaitNamedPipe`/`ERROR_PIPE_BUSY` covers instance exhaustion. No ports at all.

**Fit against this codebase — better than it first looks:**

- **The naming problem is already solved.** `src/platform.rs:345`
  `session_mutex_name()` already builds `Local\psmux-session-{data_root_tag}-{sanitized_base}`,
  with the `PSMUX_DATA_DIR` tag folded in (issue #599) and backslashes sanitised
  because they are the kernel-object namespace separator. A pipe name is the same
  function with a different prefix. The `-L` namespace, the data-root isolation and
  the sanitisation rules all come for free.
- **Raw Win32 FFI is already the house style.** `src/platform.rs` is full of
  hand-declared `#[link(name = "kernel32")] extern "system"` blocks
  (`CreateMutexW`, `QueryFullProcessImageNameW`, `SetHandleInformation`, …). A
  named-pipe transport needs **no new crate** — `windows-sys` is already a
  dependency and only needs the `Win32_System_Pipes` feature added, or the same
  hand-declared-extern pattern used everywhere else here.
- **The inheritance scrub already exists.** `clear_inherit()`
  (`src/server/connection.rs:11`) clears `HANDLE_FLAG_INHERIT` on every socket
  handle, because inherited handles pin connections open and break the
  EOF-as-end-of-reply contract. Pipe handles need exactly the same treatment via
  exactly the same `SetHandleInformation` call.
- **Stronger identity, for free:** per-pipe DACLs and `GetNamedPipeClientProcessId`.

**The one real obstacle: there is no half-close.**
`shutdown(SD_SEND)` is a Winsock API; **named pipes have no equivalent.** The
documented teardown is `FlushFileBuffers` (which blocks until the client has read
everything) → `DisconnectNamedPipe` (which *discards* unread data) →
`CloseHandle`. The nearest analogue to a half-close is a zero-length message on a
message-mode pipe, and that is
[known to be unreliable](https://github.com/Microsoft/go-winio/issues/42) — the
zero-length message can be coalesced with the preceding one.

So Option 2 **cannot** preserve the §2.4 framing as-is. It requires giving the
one-shot protocol an explicit end-of-request marker instead of a FIN. In practice
that means the client sends its command and the server, which already knows a
one-shot request is a single line, replies and closes — dropping the 10 ms drain
window (`src/server/connection.rs:926`) in favour of an explicit
`ONESHOT`/length-prefixed handshake. **This is the same protocol work Option 6
needs**, which is why the two compose so well: do Option 6 first on TCP, prove
the framing, then move the framed protocol onto pipes.

`FlushFileBuffers`-before-`DisconnectNamedPipe` is the correct server-side
discipline and is a genuinely better "reply fully delivered" guarantee than
close-as-EOF, because it blocks until the client has actually read the bytes.

**Code changes.** §2.5 refactor; a `platform::pipe` module (~200–300 lines of
FFI: create/connect/accept-loop/read/write/flush/disconnect); pipe naming via the
existing `session_mutex_name` pattern; explicit request/response framing;
`.pipe` publication alongside `.port`; `cross_session_server.rs` and
`proxy_pane.rs` ported.

**Effort:** ~6–9 days after the refactor. The largest of the options, and the
only one that ends the problem class rather than moving it.
**Risk:** medium. The FFI is well-trodden and the accept-loop race
(create-next-instance-before-current-client-connects) is a known, testable
pattern. The framing change is the risky part — and it is risky in *exactly* the
way PR #464 was, so it needs #464's test (`tests/test_command_reliability.ps1`)
as the gate.
**Ships without breaking live sessions?** Yes, with dual-listen (§6).

### Option 3 — client-side reuse / pooling / a persistent local proxy

**This option is weaker than it sounds, for a structural reason.**

Each `tmux capture-pane` is a **separate short-lived process**. There is no
in-process connection to pool — the process exits microseconds after its single
command. **Client-side pooling therefore buys nothing at all.** The only variant
that helps is the *persistent local proxy*: a long-lived per-user agent holding
one connection per session server, with every CLI invocation talking to the
agent instead.

But then: **how does the CLI reach the agent?** Over loopback TCP — the exact
problem we started with, now with an extra hop — or over a named pipe, i.e.
Option 2 plus an additional daemon, an additional lifecycle, an additional
crash-recovery story, and an additional thing that can wedge. The proxy also has
to fan out to 12+ session servers and multiplex replies, which needs the framing
work anyway.

The one place reuse *is* free and real: the callers making repeated calls could
hold a `PERSISTENT`/control-mode connection open, and **that is a caller-side
change, not a psmux change** — it is Option 5 in disguise, and it belongs with
the work the other agent is doing.

**Code changes.** Either nothing in psmux (caller holds a control-mode session),
or a whole new daemon.
**Effort:** ~1 day for the reply-framing needed to make `PERSISTENT` usable for
multi-line replies without registering a phantom client; ~10+ days for a real
proxy daemon.
**Risk:** high for the daemon variant — it adds a new single point of failure in
front of every CLI call, on a machine where the *failure* mode under discussion
already destroys sessions.
**Ships without breaking live sessions?** The framing part, yes. The daemon,
yes, but it is the most operationally invasive option here.

**Verdict: do not build the proxy.** Fold the useful half into Option 6 and the
caller work.

### Option 4 — socket-option mitigations

Being explicit about which of these is engineering and which is a trade:

| Mitigation | Verdict |
|---|---|
| **`SO_REUSEADDR` on the client** | **Does not help, and is not what people think on Windows.** It addresses binding a *listening* socket to an address in TIME_WAIT. Our failure is the ephemeral allocator having no free port for an outbound `connect()`. On Windows `SO_REUSEADDR` additionally permits genuine port hijacking, which is why `SO_EXCLUSIVEADDRUSE` exists. Wrong tool. |
| **`SO_LINGER(0)` — abortive close** | **Trades correctness for headroom.** It sends RST instead of FIN, so the sender skips TIME_WAIT. But the client is the *first* closer here, and an RST can discard data the peer has not yet read — which is precisely the failure mode PR #464 was written to eliminate. Setting it after the reply is fully read is timing-delicate and would not reliably retract a TIME_WAIT already entered. **Do not do this.** |
| **Widen the dynamic port range** (`netsh int ipv4 set dynamicport tcp start=10000 num=55535`) | **Legitimate, but it is a host configuration change, not a psmux change.** It multiplies headroom ~3.4× and buys time. It does not scale with the caller traffic and it cannot be shipped in the product. Reasonable as an immediate operational stopgap on this machine; not an answer. |
| **Lower `TcpTimedWaitDelay`** (registry, down to 30 s) | Same category: real, host-level, ~4× headroom, not shippable, and it weakens the protection TIME_WAIT provides against delayed duplicates. Stopgap only. |

**Effort:** minutes (host config).
**Risk:** low for the two host-config knobs; **unacceptable** for `SO_LINGER(0)`.
**Ships without breaking live sessions?** The host knobs take effect for new
connections without restarting servers — the only option here that requires
touching nothing at all.

### Option 5 — do nothing in psmux; fix only the callers

**When this is the right call:** if ~90 connections/sec is *itself* the anomaly
rather than the load psmux is meant to carry. A 40-spawn/sec sustained CLI poll
is an unusual way to drive a multiplexer; tmux users do not generate it, and the
Cletus bug means the failures are being amplified into session deletion. If the
callers drop to, say, 2 spawns/sec, utilisation falls to ~4% and the incident is
simply over — with **zero risk to 12 live sessions holding ~70 agents.**

**The honest case for it:** psmux's transport is not *wrong*. It is a per-command
connection model, which is what tmux effectively has too (tmux opens its Unix
socket per command); tmux just does not pay a port for it. Nothing here is
mismanaged — it is a Windows-specific cost that only shows up under machine-gun
CLI traffic.

**The honest case against:** the ceiling is now known and it is low —
~135 connections/sec sustained is all the default ephemeral range can absorb at a
2-minute retention, and psmux gives no back-pressure or clear error before it
falls off the cliff. Anything that polls psmux hard hits this again. And the
port-reuse identity hazard (§2.6a) stays.

**Effort:** zero (in psmux).
**Risk:** zero, immediately; unbounded recurrence risk later.
**Ships without breaking live sessions?** Trivially.

### Option 6 — stop half-closing; let the *server* be the active closer *(not in the original brief; found while reading)*

**How it works.** Delete the client's `shutdown(Shutdown::Write)` in
`send_control` / `send_control_with_response`. The client writes its command and
just reads. The server already ends a one-shot connection on its own — after the
command it sets a 10 ms read timeout (`src/server/connection.rs:926`), the next
`read_line` times out, the loop breaks, `handle_connection` returns and the
stream drops. **The server sends FIN first**, so the server becomes the active
closer, its TIME_WAIT sits on its **fixed listening port** (which consumes no
ephemeral port), and the client's ephemeral port is released at once through a
passive close.

**What it costs and what it risks — honestly:**

- **+~10 ms latency per CLI command**, because the client no longer signals EOF
  and the server waits out its batch-drain window. Fixable by having the client
  declare a one-shot request explicitly (a `ONESHOT\n` line after `AUTH`, or a
  length-prefixed request) so the server can close immediately — that is a small,
  additive, backwards-compatible protocol change, and it is the **same framing
  work Option 2 needs**.
- **It must not reintroduce #464.** The RST that #464 fixed came from the client
  closing *with unread data pending*. Here the client stops closing early
  altogether and reads until the server closes, so the RST window is not
  reopened — **but this is precisely the claim that must be proven by
  `tests/test_command_reliability.ps1` before it ships**, and I have not run it.
- **A second-order collision risk remains.** TIME_WAIT moves to the server, keyed
  on the 4-tuple *(server port, client ephemeral port)*. If the client's allocator
  later reuses the same ephemeral port toward the same server within the window,
  the connect can still fail with 10048. *Rough arithmetic, flagged as inference:*
  ~900 connections per 2-minute window per busy server over 16,384 ports gives on
  the order of tens of potential tuple repeats per window — much better than 87%
  exhaustion, but **not zero**. This is why Option 6 is a strong mitigation and
  not the destination.

**Code changes.** Two `shutdown` calls removed in `src/session.rs` (~1976 and the
`send_control` equivalent ~1930), plus — to avoid the 10 ms tax — an additive
`ONESHOT` handshake in `src/server/connection.rs` and the two client helpers.
`src/cross_session.rs:35` gets the same treatment.

**Effort:** ~0.5 day for the bare change; ~2 days with the `ONESHOT` framing and
tests.
**Risk:** low–medium. It touches the exact code path PR #464 fixed, so it lives
or dies on that PR's regression test.
**Ships without breaking live sessions?** **Yes, and uniquely well** — it is
purely client-side if shipped without `ONESHOT`, so a new client talks to the
*existing running servers* with no server restart at all. That makes it the only
option that improves the situation for Kevin's 12 currently-live sessions
without restarting them.

---

## 6. Compatibility and migration

**The core constraint: a running server cannot grow a new listener.** Kevin has
12 live sessions with ~70 agents. Whatever we ship, those servers keep speaking
TCP-only until they are restarted, which will not be soon. Therefore:

**Dual-listen is mandatory, not optional.**

1. **Server:** bind the TCP listener exactly as today *and* create the named pipe
   (or AF_UNIX socket). Publish `.port` exactly as today *and* a new
   `.pipe` file naming the endpoint. Accept on both; `handle_connection` becomes
   generic over the stream type and is otherwise untouched.
2. **Client:** prefer `.pipe`; **fall back to `.port` whenever `.pipe` is absent
   or the connect fails.** An old server (no `.pipe` file) is transparently
   handled by the fallback — which is what keeps the 12 live sessions working.
3. **Old client, new server:** works unchanged, because `.port` is still
   published and TCP is still accepted.
4. **Retire TCP only when** no `.port`-only servers can plausibly still be
   running — realistically a release or two later, and arguably never, since the
   TCP path is also what `docs/diagnostics.md` and the test suite lean on.

**The `.port` file is a public contract.** It is documented at
`docs/diagnostics.md:121` and **read directly by 446 files under `tests/`**,
including `tests/psmux_test_helpers.ps1` and the whole `tests/bench/` suite.
Keeping `.port` published during dual-listen means **the test suite needs no
migration at all**, which removes what would otherwise be the single largest cost
of this project. Any plan that drops `.port` on day one is a plan to rewrite 446
test scripts.

Two smaller compatibility notes:

- **`-L` namespacing already works** for pipe names via
  `session_mutex_name`'s existing `data_root_tag()` scheme (§5, Option 2) — no new
  namespace design needed.
- **Non-Windows builds:** the tree already has `#[cfg(windows)]` / `#[cfg(not(windows))]`
  pairs throughout `platform.rs` (e.g. `acquire_session_mutex` fails open off
  Windows). A pipe transport needs the same treatment, or TCP stays the
  non-Windows path.

---

## 7. Deployment: getting a fork build onto this machine without losing sessions

This section is a hard prerequisite for *any* of the options above, and it
contains the mechanism behind the previous 7-session loss.

### 7.1 Why renaming a running psmux binary destroys sessions — confirmed

Windows refuses to **overwrite** a running `.exe` (sharing violation) but happily
allows it to be **renamed**. Renaming changes the live process's image path, and
psmux's liveness check reads that path back:

```rust
// src/platform.rs:3061 — get_process_name
let name = std::path::Path::new(&full_path).file_stem()…   // QueryFullProcessImageNameW

// src/session.rs:639
const PSMUX_SERVER_IMAGE_NAMES: &[&str] = &["psmux", "tmux", "pmux"];

// src/session.rs:1165 — registry_pid_anchor_alive
if !PSMUX_SERVER_IMAGE_NAMES.contains(&name.as_str()) {
    // PID recycled by an unrelated application; our server is gone.
    return Some(false);
}
```

`Path::new("psmux.exe.old").file_stem()` is **`"psmux.exe"`**, which is not in
that list. So the instant a running `psmux.exe` is renamed to `psmux.exe.old`,
every liveness check reports **"our server is gone"**, and
`remove_session_registry_files` (`src/session.rs:1331`) deletes the `.port`,
`.key`, `.sid`, `.pid` and `.act` files of servers that are still running
perfectly well. The sessions become unreachable, and the reaper then skips those
same processes as "not psmux" — so they linger as orphans with no registry.

The install directory shows exactly this naming in its history:
`psmux.exe.old`, `psmux.exe.old-20260703-registryfix`, `tmux.exe.bak-20260823`,
alongside six `backup-*/` directories. **`.exe.old` is the fatal spelling.**
(A rename to `psmux.bak` would coincidentally keep the stem `psmux` and survive —
but relying on that is far too subtle to be a procedure.)

### 7.2 Landmine: `scripts/build.ps1` will kill every session

```powershell
# scripts/build.ps1:17-25
& psmux kill-server 2>$null
foreach ($name in @("psmux", "pmux", "tmux")) {
    Get-Process -Name $name | Stop-Process -Force
}
```

**Do not run `scripts/build.ps1` on this machine.** It calls the bare
`psmux kill-server` that `AGENTS.md` forbids, and then force-kills all three
image names. `scripts/install.ps1:99` is milder — `Copy-Item -Force` — but it
will simply fail with a sharing violation against a running `psmux.exe`, and any
attempt to "fix" that failure by renaming the target is §7.1.

### 7.3 The three binaries

`Cargo.toml` declares three `[[bin]]` targets — `psmux`, `pmux`, `tmux` — all
with `path = "src/main.rs"`. They are **not copies**; verified by hash on the
installed build:

```
875ee871…  psmux.exe
6d3fe9bf…  pmux.exe   (same size, different content)
761efa5e…  tmux.exe
```

They differ because cargo embeds the binary name, and psmux's own behaviour keys
off `argv[0]` in places. **They must be deployed as a matched set** — a mixed set
means a `tmux.exe` client and a `psmux.exe` server from different commits, which
is exactly the client/server protocol skew that a transport change makes fatal.
All three names are in `PSMUX_SERVER_IMAGE_NAMES`, so all three can be a server.

### 7.4 The safe deployment procedure

The installed build is winget-managed (`marlocarlo.psmux`, 3.3.8) at
`C:\Users\1010\AppData\Local\psmux\`, and right now **14 `psmux.exe` and 7
`tmux.exe` processes are running out of that exact directory.**

**Recommended: side-by-side versioned directory, no mutation of the live one.**

1. `cargo build --release` in the repo (not `cargo install`, not `build.ps1`).
   Do this when the machine is not loaded.
2. Copy all three exes to a **new** directory, e.g.
   `C:\Users\1010\AppData\Local\psmux-<shorthash>\`. Nothing in the live
   directory is touched, so no running process's image path changes and no
   liveness check trips.
3. Put the new directory **ahead of** the winget directory on `PATH` for new
   shells only. Existing sessions and their agents keep resolving the old binary;
   new invocations get the new one.
4. Because of dual-listen (§6), a new client transparently falls back to `.port`
   for the 12 old servers, and uses the pipe for any server started afterwards.
   **No session needs to be restarted at any point.**
5. Retire the old directory only once `Get-Process` shows nothing running from
   it.

This also sidesteps winget: a fork build inside the winget directory would be
silently clobbered by the next `winget upgrade`, and cannot be written at all
while servers run. If the fork is ever meant to *replace* the winget install, the
only safe sequence is to drain every session first — which is not on the table
here.

---

## 8. Recommendation

**Ship Option 6 now, and Option 2 as the destination.**

Option 6 — stop the client half-close so the server becomes the active closer —
is the only change that helps the 12 sessions that are live *today*, because in
its minimal form it is purely client-side and needs no server restart. It removes
the client-side TIME_WAIT accumulation that is causing the 87% exhaustion, it is
half a day of work plus PR #464's regression test as the gate, and its `ONESHOT`
framing refinement is the same protocol work Option 2 needs anyway — so it is not
throwaway. It is a mitigation, not a cure: it relocates TIME_WAIT to the server's
fixed port and leaves a smaller 4-tuple-collision risk. Option 2, Windows named
pipes, is the cure, because it deletes the port concept entirely and takes the
port-reuse identity-confusion class (§2.6a) with it; it fits this codebase far
better than its reputation suggests, since the naming scheme already exists in
`session_mutex_name`, the raw-Win32 idiom is already the house style, and no new
crate is required. I would **not** choose Option 1: on Windows-only software it
buys the same thing as named pipes while adding a third-party crate to the
critical path and losing `connect_timeout`, which is load-bearing after #464. I
would **not** build the Option 3 proxy: per-invocation processes make pooling
structurally useless, and the daemon variant adds a new single point of failure
in front of every CLI call. Option 5 remains correct on its own terms and should
happen regardless — the caller traffic is the actual anomaly — and the host-level
port-range widening from Option 4 is a fine stopgap for tonight, provided nobody
reaches for `SO_LINGER(0)`.

**Sequencing:** (a) widen the dynamic port range as an immediate host stopgap;
(b) land the caller fixes already in flight; (c) do the `trait ControlStream`
refactor from §2.5, which is a prerequisite for everything and reviewable on its
own; (d) Option 6 with `ONESHOT` framing; (e) Option 2 behind dual-listen.
Steps (c)–(e) are roughly 11–15 engineer-days total.

## 9. The biggest open question I could not resolve

**Does removing the client's half-close actually stay clear of the RST that PR
#464 fixed?** My reasoning in §5/Option 6 is that #464's RST came from the client
closing *with unread data pending*, and that a client which never closes early
cannot reproduce it — but that is a code-reading argument, and I deliberately did
not build or run anything on this machine. The claim is exactly the kind that
Windows loopback semantics punish for being plausible. It needs
`tests/test_command_reliability.ps1` (the test PR #464 shipped) run against a
patched client in a disposable namespace per `AGENTS.md`, and until it passes,
the whole recommended sequencing rests on an unverified premise.

Two smaller unknowns, both flagged inline: the true rate of 4-tuple collisions
after Option 6 (my arithmetic is order-of-magnitude only), and whether the
current port exhaustion has *already* been causing stale-`.port`-points-at-a-
stranger incidents (§2.6a) — which, if it has, would raise the priority of
Options 1/2 over Option 6 considerably.

---

## Appendix: sources

Code (this repo, at `a186289`): `src/server/mod.rs`, `src/server/connection.rs`,
`src/session.rs`, `src/client.rs`, `src/main.rs`, `src/paths.rs`,
`src/platform.rs`, `src/cross_session.rs`, `src/cross_session_server.rs`,
`src/proxy_pane.rs`, `Cargo.toml`, `scripts/build.ps1`, `scripts/install.ps1`,
`tests/bench/bench_ipc_compare.ps1`, `docs/diagnostics.md`.

External:
- [rust-lang/rust#147335 — `std::os::windows::net` Unix domain sockets](https://github.com/rust-lang/rust/pull/147335) (closed, licensing)
- [`uds_windows` on lib.rs](https://lib.rs/crates/uds_windows) and [`UnixStream` API docs](https://docs.rs/uds_windows/latest/uds_windows/struct.UnixStream.html)
- [Named Pipe Operations — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-operations)
- [microsoft/go-winio#42 — zero-length message EOF unreliability](https://github.com/Microsoft/go-winio/issues/42)
- psmux PR #464, "fix(ipc): make one-shot CLI commands reliable"
