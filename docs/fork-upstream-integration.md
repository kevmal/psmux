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

## PR #7 target safety is ported onto the upstream protocol

The first draft reproduced the existing wrong-window bug fixed by
[PR #7](https://github.com/kevmal/psmux/pull/7): `kill-window -t gamma:%2`
removed active `%1` and left `%2`. The follow-up now ports #7's semantics
without changing its branch or resurrecting `FocusTargetTemp`.

Window mutations and window-option commands retain their complete raw target.
`ResolveWindowTarget` resolves that spec read-only into a stable window id;
the existing `TargetedSender`/`CtrlReq::Targeted` carries it with each request.
`KillWindowTarget` also keeps the raw spec and resolves it on live state in
the mutation turn. Missing/malformed pane components cannot silently become
an active-window operation. Qualified session identity is checked, numeric
pane indexes honor pane-base-index, and floating pane ids map to their owner.
Resize-window retains composite syntax through its `Spec` variant.

The port carries #7's pure resolver/resize cases and full runtime safety suite,
adapted to a private data root, empty config, unique `-L`, LF wire framing and
filesystem-only default-registry snapshots. All **187** runtime checks pass:
valid pane-to-window mutation, survivor focus, invalid/malformed targets,
window options and CLI/raw/control paths. The CI test job runs this suite
against its freshly built binary before the full Rust suite.

The separate #7 commit is not a parent of this branch; its behavior is ported
and tested here. #7 can be reconciled/closed only after review of this concrete
combined implementation. Keep this PR draft until Kevin's thorough review;
green CI does not authorize merging master or deploying binaries.

The independent review also found three CI fixture failures. The parity test
now expects the retained 500 ms; both source-job TCP peers read framed AUTH
and command lines instead of waiting for a client half-close. Production
passive close and refusal classification remain unchanged.

### Second review: relative targets and focus ownership

The green `5b9dcda2` candidate was still unsafe: resolving `session:+` for
temporary focus and then resolving the original string inside kill-window or
unlink-window advanced twice. The revised sender puts the **same validated
stable `@id`** into both the wrapper and the self-resolving request. The server
still checks that id at execution time; disappearance cannot select the active
window instead. Window option writes, value reads and listings share this fix.
The global window-option table sentinel is preserved.

The related command audit found these ownership rules:

| Commands | Target/focus ownership |
|---|---|
| kill-window, unlink-window, window option reads/writes | Validate the whole spec once, pin the carried target to that stable id, apply/execute/restore atomically. |
| rename-window, rotate-window, layout commands, respawn-window | Operate on current window inside the stable wrapper; no second raw-target resolver. No respawn was executed locally. |
| select-window | Its own complete resolver and persistent focus operation; generic temporary focus is excluded. |
| resize-window | Its own full-spec resolver; generic temporary focus is excluded. |
| move/swap/link-window, join/move/break-pane, explicit swap-pane | Resolve both ends using original focus. The sender never temporarily focuses their destination before resolving an implicit source, including dispatch from an if-shell branch. |
| select-pane | Explicit target selection locates the start; relative/directional selection runs once. Control mode now performs the relative operation instead of silently acknowledging a no-op. |
| send/paste, pane attributes/options, pane resizing | Stable pane wrapper; their scoped operations do not advance a relative window spec again. Existing unsupported raw syntax is not broadened. |
| capture/display, pane-id kill | Their explicit pane-id paths check identity and do not retarget a vanished pane. |

Cross-window swap-pane previously allowed the generic window focus to replace
its implicit source; both entry paths now exclude that focus, and the sender
also protects multi-target requests from accidental wrapping. The internal
`session-info` barrier ignores inherited window focus, so a successful relative
window-option write is not followed by a spurious target-name error.

Window commands accept `:+.+`/`:-.-` as their relative pane's containing window.
**kill-pane still does not implement relative pane selectors**: it rejects
`:.+`/`:.-` before the parser can discard the suffix and kill the active pane.
**Control mode still has no move-window dispatcher**; its explicit error and
unchanged state are tested. Neither limitation is represented as working parity.

`tests/test_relative_target_safety.py` exercises three-window destruction with
`:+`, `:-`, offsets, `{next}`, `{previous}`, `{start}`, `{end}`, `!`, `{last}`
and relative composite pane targets through CLI, raw LF TARGET and control
mode. It also checks rename, option write/read, selection, relative pane
selection/rejection, cross-window swaps and move-window source identity. It
uses only private namespaces/config/data and default-registry file snapshots.
Pure tests exercise resolve/wrap/focus/re-resolve and target disappearance.

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

`--probe-open-pr7` is a focused regression: it reports the requested and
remaining pane ids and now asserts that the correct window was removed.

237 focused Rust tests passed after the relative-target correction. They cover passive closes
(including warm claim and factored
exchange), pending-read classification, stable-target wrapping/restore,
profile/shim precedence, compiled defaults and option reset parity. They use
pure fixtures or private loopback peers; inspect filters before running them.
Do not run `cargo test --all-targets` on a live workstation. Full-suite and
cross-architecture results must come from disposable CI runners, and shared
server mode requires separate acceptance before it is ever enabled.
