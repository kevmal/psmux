# Fork integration of upstream 7a109f9a (2026-10-23)

This branch merges the full upstream history through `7a109f9a` into fork
`144a46c9`: 1,114 commits, including 899 non-merge commits. It is a source-only
PR for Kevin's review, not approval to merge to master or deploy a binary.
The upstream id-allocator change `13064512` and its revert `855da885` are both
included. Fork feature commit `065d97a` is not an ancestor of this branch.

## Defaults retained from the fork

| Option | This fork | Upstream tip | Reason |
|---|---|---|---|
| `warm-pool-size` | `1` | `3` | Preserve the previous single `Option<WarmPane>` spare and idle memory cost. Larger pools remain configurable. |
| `escape-time` | `500` ms | `10` ms | Preserve the configured default; upstream's new input decoding/ambiguous-Escape ceiling still applies. |
| `set-clipboard` | `on` | `external` | Preserve application OSC 52 paste-buffer behavior. |
| `server-mode` | `per-session` | `per-session` | Shared mode remains opt-in; this integration does not enable it or modify any user configuration. |

The compiled defaults, option catalog, help, reset expectations and user docs
agree. Upstream performance measurements assuming three ready spare shells
are not promises for the fork's one-spare default.

## Conflict and IPC decisions

- Keep upstream's `is_read_retry` at all three persistent-reader sites. It
  includes the fork's raw Windows error 997 case, plus interrupted reads,
  10004 and 10060, while disconnects remain fatal. Remove the narrower helper.
- Combine upstream profile-defined `claude`/`env` precedence with the fork's
  explicit `claude.exe`, `claude.cmd`, `claude.ps1` resolution order and useful
  missing-command error. Keep settings precedence and scoped agent-team values.
- Keep upstream Meta-Space/Meta-Escape and modifier-order handling together
  with fork Enter/Return/CR aliases.
- Keep passive closes in `send_control` and the refactored response path
  `exchange_on_port`, newline normalization, reply-to-EOF and timeout/error
  classification. The borrowed-string signature of `exchange_on_port` needs
  `Cow<str>` normalization when incorporating the old String-based guard.
- Omit upstream `096a0e6e`'s client `shutdown(Write)` in
  `send_auth_cmd_response`. The helper waits for server EOF, accepting the
  small batch-read tail rather than reintroducing a client active close.
  No port/TIME_WAIT claim is made for the omitted optimization.
- Preserve non-inheritable ConPTY pipes, unconditional attach-client TERM
  initialization, forced color output, and qualified `session:%N` parsing.

Upstream `e9b9bcce` independently fixes the race addressed by fork `065d97a`:
each `CtrlReq::Targeted` carries stable target identity, and the event loop
applies/restores focus within the request. The old fork commit is deliberately
not cherry-picked on top. Both ordinary and stalled concurrent sends must be
checked against the new implementation.

## PR #7 is an unresolved dependency

[PR #7](https://github.com/kevmal/psmux/pull/7), head `62c2f69c` at review time,
remains on its separate branch. It fixes pane targets being lost before window
mutations. Upstream request-local targeting does **not** fix that entire issue:
`kill-window` still skips generic pane focus and its `KillWindowTarget` arm only
receives the parsed window portion.

The isolated diagnostic reproduces it: create gamma with active pane `%1` and
another window holding `%2`; `kill-window -t gamma:%2` leaves `%2` and removes
`%1`. Both panes are disposable probes in the test's private `-L` namespace.
This is the existing bug fixed by #7, not a claim of passing target safety.

**Do not merge this PR independently ahead of #7's semantics.** Whichever PR
lands second needs a semantic rebase. Keep upstream `TargetedSender` and
`CtrlReq::Targeted`; do not resurrect the old `FocusTargetTemp` protocol while
porting #7. Carry #7's raw-window-target resolution, pane-to-owning-window
mapping, fail-closed behavior and resize/option handling onto the new resolvers
and both one-shot/control dispatch paths. Run #7's window-target regression
suite against the combined tree before approval. This branch does not modify
#7's branch or absorb its commit.

## Validation and safety boundaries

`tests/test_fork_upstream_contract.py` accepts an explicit worktree binary,
creates a unique `-L` namespace and private `PSMUX_DATA_DIR`, and supplies an
empty config. It disables warm spares for these focused delivery checks.
It never calls `kill-server`, installs/copies a psmux binary, or sends commands
to the live default server. Cleanup stops only its own probes/sessions; default
registry session names and identity anchors are snapshotted read-only and
compared before/after. Its JSON artifact records each command, exit, payload
log, namespace and cleanup result without recording authentication keys.

The normal run checks colliding pane ids in two sessions; qualified targets;
concurrent sends with capture/list traffic; repeated keys; 1.5-second and
6.2-second dispatch stalls; unchanged focus; Anjara AgentLauncher's LF
AUTH/TARGET/base64-paste immediate-close path; `capture-pane -e -p`;
Enter/Return/CR/Escape bytes; and removed/invalid targets with no redirection.
Long-stalled read queries may report their bounded timeout; the send payloads
must still arrive exactly once in only the intended panes.

`--probe-open-pr7` is a separate diagnostic, deliberately reporting whether the
known window-target bug remains; its exit code is not a target-safety verdict.

Focused Rust tests cover passive closes (including warm claim and factored
exchange), pending-read classification, stable-target wrapping/restore,
profile/shim precedence, compiled defaults and option reset parity. They use
pure fixtures or private loopback peers; inspect filters before running them.
Do not run `cargo test --all-targets` on a live workstation. Full-suite and
cross-architecture results must come from disposable CI runners, and shared
server mode requires separate acceptance before it is ever enabled.
