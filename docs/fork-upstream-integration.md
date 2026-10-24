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
| rename-window, rotate-window, layout commands, respawn-window | Operate on current window inside the stable wrapper; no second raw-target resolver. No local process was respawned; private rejection probes only. |
| select-window | Its own complete resolver and persistent focus operation; generic temporary focus is excluded. |
| resize-window | Its own full-spec resolver; generic temporary focus is excluded. |
| move/swap/link-window, join/move/break-pane, explicit swap-pane | Resolve both ends using original focus. The sender never temporarily focuses their destination before resolving an implicit source, including dispatch from an if-shell branch. |
| select-pane | Supported +/- selectors are resolved read-only to a stable %id before dispatch; other relative/symbolic selectors are refused. Directional flags remain supported. |
| send/paste, pane attributes/options, pane resizing | Stable pane wrapper; their scoped operations do not advance a relative window spec again. Existing unsupported raw syntax is not broadened. |
| capture/display, pane-id kill | Their explicit pane-id paths check identity and do not retarget a vanished pane. |

Cross-window swap-pane previously allowed the generic window focus to replace
its implicit source; both entry paths now exclude that focus, and the sender
also protects multi-target requests from accidental wrapping. The internal
`session-info` barrier ignores inherited window focus, so a successful relative
window-option write is not followed by a spurious target-name error.

Window commands accept `:+.+`/`:-.-` as their relative pane's containing window.
**Pane-scoped commands reject unsupported relative/symbolic pane selectors**
before the parser can discard the suffix and operate on the current pane.
**Control mode still has no move-window dispatcher**; its explicit error and
unchanged state are tested. Neither limitation is represented as working parity.

`tests/test_relative_target_safety.py` exercises three-window destruction with
`:+`, `:-`, offsets, `{next}`, `{previous}`, `{start}`, `{end}`, `!`, `{last}`
and relative composite pane targets through CLI, raw LF TARGET and control
mode. It also checks rename, option write/read, selection, stable relative
pane selection, rejection, cross-window swaps and move-window source identity. It
uses only private namespaces/config/data and default-registry file snapshots.
Pure tests exercise resolve/wrap/focus/re-resolve and target disappearance.

### Pane selectors resolve before parsing and mutation

Independent review reproduced relative pane respawn/option operations falling
back to the current pane, and malformed `%bad` / `@id.%bad` ids disappearing
inside the permissive numeric parser. A temporary blanket refusal policy was
insufficient because valid navigation and config targets must keep working.

All supported explicit pane endpoints now resolve read-only to stable `%id`
values before legacy parsing. A single immutable server snapshot resolves both
source and destination together for swaps/joins/moves, including an omitted
endpoint. Every command then uses those ids; no relative selector is evaluated
a second time after temporary request focus. A deleted id remains an error.
An empty window with `.%id` follows the pane to its owning window; an explicit
`@window.%id` must match that owner.

The resolver supports +/- offsets, next/previous/last aliases, marked targets,
corners/edges and directional selectors with existing layout helpers. Numeric
indices respect pane-base-index. Explicit sessions are validated against the
server reached; colliding ids in another session cannot be substituted.
Mouse `=`/`{mouse}` is converted to the mouse event's stable pane by the existing
mouse binding preprocessing; without an event, it reports missing context.
Malformed ids, invalid offsets and unsupported/ambiguous fields fail before
mutation. Window-only commands keep the separate complete window resolver.
The target grammar follows [tmux cmd-find.c](https://github.com/tmux/tmux/blob/master/cmd-find.c)
and [window.c](https://github.com/tmux/tmux/blob/master/window.c); this uses
psmux's existing geometry and direction code.

The shared ingress walk uses each command's getopt template. Aliases,
clustered/attached flags, value-taking flags, `--`, titles, option values and
shell operands retain their proper boundaries. It covers pane process
operations, options/attributes/hooks, input/paste, capture/display,
resize/split, pipes/history/modes, pane UI/internal commands, context for
run-shell/if-shell/source-file and all pane source `-s` endpoints. Well-formed
missing CANFAIL context for run/if/source keeps its existing no-target format
semantics, while malformed fields are errors. Nested control branches normalize
before dispatch too. In-server binding/config execution resolves from AppState
and forwards targeted focus handlers over the existing canonical command path;
commands that own their targets locally retain their direct implementation.
Control dispatch now also handles pipe-pane, clear-history, send-prefix,
clock-mode and set-pane-title using the same backend requests as one-shot mode.

### Binding compatibility

No user config or binding was edited. The canonical user .tmux.conf was
inspected: its launcher, tag, find-window, pane-naming and session-menu bindings
contain no explicit relative pane targets. Prompt values and shell operands
remain data. Prefix-o's `select-pane -t :.+` resolves once to the next stable
pane; default untargeted swaps, directional navigation, resize, zoom,
splits and copy/paste retain their handlers. Mouse event targets are expanded
before this resolver. Focused checks cover prefix-o at the wire and in-server
binding entry points, default swap/rotate inverses, nested commands and mouse
normalization. This is not a claim that every external popup/script or host
keyboard/terminal combination was exercised.

The old 187-check fixture now waits at most five seconds for its specific
last-pane victim window to disappear after child termination and dead-PTY
cleanup, keeping its original correct-victim/active-survivor assertions.
Production completion and EOF behavior are unchanged.

### Attached/clustered flags and all command ingress

Independent review found attached primary/source flags still bypassed later
exact-token scans. This affected valid input delivery and window/pane movers,
not merely malformed errors. Every routing/dispatch path now receives one
canonical getopt form: separate, attached, clustered and =value flags share
flag/value tokens, and source -s is handled alongside primary -t. The CLI does
this before choosing a session. Raw/control, aliases, nested branches and
in-server execution do it before target extraction. The control send coalescer
also canonicalizes before it can merge any input; otherwise it could discard
a long target before ordinary dispatch saw it.

Internal pane extensions without a tmux catalog template use the same
target/source grammar for argument validation as for canonicalization.
Config option-name and scope errors retain their established diagnostic
precedence before read-only target lookup. Unknown long targets and missing
-t arguments fail before zoom,
input or other internal requests can mutate current state.

--target and --source (separate or =value) are compatible aliases of -t/-s
where those flags take values. Unsupported long flags report an error.
Required/optional value boundaries, -- and first-operand boundaries follow
the command templates. Target-shaped titles, option values and shell operands
remain data. The legacy helper that only normalizes attached -t retains its
existing public test contract; the actual CLI uses the common full pass.

Window option scope uses ParsedSetOptionArgs, rather than re-guessing letters
inside a raw flag/value token. Config option writes use that same parsed scope
and target, with original token offsets for quoted value text. Explicit config
pane/window targets resolve to stable ids before writes, so attached target
text cannot become scope flags or an option name.

The preceding full CI exposed real local-execution regressions: unconditional
requotes lost quoted format values and copy-prompt template syntax. Raw tails
now retain their original quoting unless pane identity actually changes; a
rewritten format token is quoted. Well-formed missing UI targets set an error
status without mutation, and untargeted empty embedded break-pane remains a
no-op. Malformed pane fields still fail before mutation. The valid-session
display test now supplies a real stub pane instead of an empty split. The npm
wrapper fixture excludes broken exe symlinks from its synthetic npm-only PATH,
because PowerShell discovers such links even when Rust exists() follows them
to a missing target. No installed shim was changed.

The attached-target runtime suite covers both input consumers (send and paste),
primary/source failures, window scope, exact victim identity, nested dispatch,
in-server commands, long forms and flag-shaped data. Full CI includes it with
the prior target suites. No user binding or configuration was edited. Swap-window -d retains the
baseline 144a46c destination-selection rule, matching current
[tmux cmd-swap-window.c](https://github.com/tmux/tmux/blob/master/cmd-swap-window.c).
An unrelated active window is not preserved by that command; the explicit
source, exchanged window identities and PIDs are the routing oracle.

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

666 focused Rust checks passed after the getopt and raw-tail corrections. They cover passive closes
(including warm claim and factored
exchange), pending-read classification, stable-target wrapping/restore,
profile/shim precedence, compiled defaults and option reset parity. They use
pure fixtures or private loopback peers; inspect filters before running them.
Do not run `cargo test --all-targets` on a live workstation. Full-suite and
cross-architecture results must come from disposable CI runners, and shared
server mode requires separate acceptance before it is ever enabled.
