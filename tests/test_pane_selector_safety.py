"""Fail-closed pane selector matrix in a unique -L and private data/config root.

Every command must fail before replacing processes, writing input/options,
moving panes or changing focus. Only this worktree binary is used. No install,
binary copying, kill-server or live-server command is permitted.
"""

import argparse
import json
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
from test_relative_target_safety import RelativeTargets
from test_fork_upstream_contract import default_registry_snapshot, wait_for


COMMANDS = [
    ("respawn-pane", ["-k", "cmd.exe /k"]),
    ("respawnp", ["-k", "cmd.exe /k"]),
    ("set-option", ["-p", "@pane-selector-probe", "WRONG"]),
    ("set", ["-p", "@pane-selector-probe", "WRONG"]),
    ("show-options", ["-p", "-v", "@pane-selector-probe"]),
    ("select-pane", []), ("select-pane", ["-T", "WRONG"]),
    ("select-pane", ["-P", "fg=red"]), ("select-pane", ["-d"]),
    ("select-pane", ["-e"]), ("last-pane", []),
    ("resize-pane", ["-R", "1"]), ("send-keys", ["-l", "{WRONG}"]),
    ("send-prefix", []), ("capture-pane", ["-p"]),
    ("pipe-pane", ["cat"]), ("display-message", ["-p", "#{pane_id}"]),
    ("swap-pane", ["-d"]), ("join-pane", ["-d"]), ("move-pane", ["-d"]),
    ("split-window", ["-d", "cmd.exe /k"]), ("new-pane", []),
    ("kill-pane", []), ("clear-history", []), ("copy-mode", []),
    ("clock-mode", []), ("paste-buffer", []),
    ("run-shell", ["echo WRONG"]), ("if-shell", ["-F", "1", "display-message WRONG"]),
    ("source-file", ["missing-pane-guard-test.conf"]),
    ("set-hook", ["-p", "after-select-pane", "display-message WRONG"]),
    ("show-hooks", ["-p"]),
]
COMMANDS += [(command, []) for command in (
    "choose-buffer", "choose-tree", "choose-window", "choose-session", "find-window",
    "customize-mode", "switch-mode", "display-menu", "set-pane-title", "focus-pane",
    "paste-buffer-at", "paste-buffer-named", "scroll-up", "scroll-down", "pane-scroll",
    "pane-mouse", "mouse-down", "mouse-down-right", "mouse-down-middle", "mouse-drag",
    "mouse-up", "mouse-up-right", "mouse-up-middle", "mouse-move", "copy-drag-begin",
    "mouse-key", "mouse-forward", "send-text", "send-paste", "send-key",
    "copy-mode-page-up", "copy-enter", "copy-move", "copy-anchor", "rectangle-toggle", "copy-yank",
)]


class PaneSelectors(RelativeTargets):
    def run(self, *args, check=True):
        # The delivery harness forbids respawn entirely. This dedicated test
        # permits only explicit relative/symbolic-target rejection probes in
        # its own private namespace, never a concrete or default target.
        if args and args[0] in ("respawn-pane", "respawnp"):
            assert "-t" in args
            target = args[args.index("-t") + 1]
            body = target.split(':', 1)[-1]
            assert target.startswith(("alpha:", "%", "@"))
            assert (any(s in body for s in (".+", ".-", ".!", ".{"))
                    or body == "%bad" or body.endswith(".%bad"))
            cmd = [self.binary, "-L", self.ns, *args]
            p = subprocess.run(cmd, env=self.env, capture_output=True, encoding="utf-8",
                               errors="replace", timeout=20)
            self.commands.append(dict(args=list(args), exit=p.returncode, out=p.stdout, err=p.stderr))
            if check:
                assert p.returncode == 0, (args, p.stdout, p.stderr)
            return p
        return super().run(*args, check=check)

    def bare_probe(self, route, command, target, *tail):
        if route == "cli":
            p = self.run(command, "-t", target, *tail, check=False)
            assert p.returncode != 0, (route, command, target, p.stdout, p.stderr)
        elif route == "raw":
            reply = self.wire("alpha", " ".join((command, *tail)), target).decode()
            assert "ERROR: invalid pane target" in reply, (command, target, reply)
        else:
            reply = self.control("alpha", " ".join((command, "-t", target, *tail)))
            assert "%error " in reply and "invalid pane target" in reply, (command, target, reply)


def snapshot(test):
    state = test.run("list-panes", "-s", "-t", "alpha:", "-F",
                     "#{window_id}|#{window_active}|#{window_layout}|#{pane_id}|#{pane_pid}|"
                     "#{pane_dead}|#{pane_active}|#{pane_in_mode}|#{pane_width}|#{pane_height}|"
                     "#{pane_title}|#{@pane-selector-probe}").stdout
    return state, {label: test.bytes(label) for label in test.logs}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--artifacts", default=None)
    args = parser.parse_args()
    before = default_registry_snapshot()
    test = PaneSelectors(args.binary, args.artifacts or tempfile.mkdtemp(prefix="pane-selector-results-"))
    error = None
    try:
        test.setup()
        extra = test.run("split-window", "-d", "-t", test.A, "-P", "-F", "#{pane_id}",
                         "--", *test.probe_args("E")).stdout.strip()
        wait_for(lambda: "target_routing_probe" in test.run("capture-pane", "-p", "-t", f"alpha:{extra}").stdout)
        first_window = test.run("display-message", "-p", "-t", test.A, "#{window_id}").stdout.strip()
        specs = [".+", ".-", ".+2", ".-2", ".!", ".{last}", ".{top-left}",
                 f"{first_window}.+", f"{first_window}.-", "%bad", f"{first_window}.%bad"]
        for route in ("cli", "raw", "control"):
            count = 0
            for spec in specs:
                # Reproduce both active endpoint cases from the review.
                active = f"alpha:{extra}" if spec.endswith(".-") else test.A
                test.run("select-pane", "-t", active)
                commands = COMMANDS if spec in (".+", ".-", "%bad", f"{first_window}.%bad") else [
                    (command, tail) for command, tail in COMMANDS
                    if command in ("respawn-pane", "set-option", "select-pane", "send-keys",
                                   "capture-pane", "display-message", "swap-pane", "join-pane")
                    and (command != "select-pane" or not tail)
                ]
                for command, tail in commands:
                    if command == "select-pane" and spec.rsplit('.', 1)[-1] in ("+", "-"):
                        # These forms have a read-only stable-id resolver;
                        # navigation and attributes are verified separately.
                        continue
                    initial = snapshot(test)
                    reply = test.invoke(route, "alpha", command, spec, *tail, fail=True)
                    # The control parser refuses some internal/UI commands
                    # before dispatch. An explicit unknown-command error is
                    # also fail-closed; it must still preserve all state/input.
                    assert ("unsupported relative or symbolic pane target" in reply or "invalid pane target" in reply
                            or route == "control" and f"parse error: unknown command: {command}" in reply), (route, command, spec, reply)
                    assert snapshot(test) == initial, (route, command, spec, "state/input changed after refusal")
                    test.cases.append(dict(route=route, command=command, args=tail, target=spec, ok=True))
                    count += 1
                for command in ("swap-pane", "join-pane", "move-pane", "break-pane", "copy-mode"):
                    initial = snapshot(test)
                    reply = test.invoke(route, "alpha", command, first_window,
                                        "-s", f"alpha:{spec}", fail=True)
                    assert "pane target" in reply, (route, command, spec, reply)
                    assert snapshot(test) == initial
                    test.cases.append(dict(route=route, command=command, source=spec, target=first_window, ok=True))
                    count += 1
            for malformed in ("%bad", f"{first_window}.%bad"):
                for command, tail in [("respawn-pane", ["-k", "cmd.exe /k"]),
                                      ("set-option", ["-p", "@pane-selector-probe", "WRONG"])]:
                    initial = snapshot(test)
                    test.bare_probe(route, command, malformed, *tail)
                    assert snapshot(test) == initial
                    test.cases.append(dict(route=route, command=command, bare_target=malformed, ok=True))
                    count += 1
            pane_ids = test.run("list-panes", "-t", f"alpha:{first_window}", "-F", "#{pane_id}").stdout.splitlines()
            assert len(pane_ids) == 3
            for spec, start in [(".+", 0), (".-", 2)]:
                test.run("select-pane", "-t", f"alpha:{pane_ids[start]}")
                title = f"stable_select_{route}_{start}"
                test.invoke(route, "alpha", "select-pane", spec, "-T", title)
                rows = test.run("list-panes", "-t", f"alpha:{first_window}", "-F", "#{pane_id}|#{pane_title}").stdout.splitlines()
                assert f"{pane_ids[1]}|{title}" in rows, (route, spec, rows)
                active = test.run("display-message", "-p", "-t", "alpha:", "#{pane_id}").stdout.strip()
                assert active == pane_ids[start], (route, spec, "title-only selection changed focus")
                test.cases.append(dict(route=route, command="select-pane -T resolves stable relative target", target=spec, ok=True))
                count += 1
            print(f"PASS {route}: {count} pane-scope cases; unsupported selectors refused, supported select attributes reach the stable target", flush=True)
    except BaseException as exc:
        error = repr(exc)
        raise
    finally:
        try:
            test.cleanup()
        finally:
            after = default_registry_snapshot()
            result = dict(namespace=test.ns, error=error, cases=test.cases,
                          default_registry_unchanged=before == after,
                          private_session_registry_empty=not list(test.data.glob(f"{test.ns}__*.port")))
            (test.artifacts / "results.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
            assert before == after, "default registry changed; investigate before continuing"
    print(f"Passed {len(test.cases)} pane-scope cases; default registry unchanged; private registry empty", flush=True)


if __name__ == "__main__":
    main()
