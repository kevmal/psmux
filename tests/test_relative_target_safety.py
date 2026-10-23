"""Relative window/pane routing regressions; only an explicit private -L server.

Uses the worktree binary directly, a private data/config root, and filesystem
snapshots of the default registry. Never installs, copies, respawns or runs
kill-server. Every fixture is removed with its explicit namespaced session.
"""

import argparse
import json
from pathlib import Path
import socket
import sys
import tempfile

sys.dont_write_bytecode = True
from test_fork_upstream_contract import Contract, default_registry_snapshot


class RelativeTargets(Contract):
    def __init__(self, binary, artifacts):
        super().__init__(binary, artifacts)
        self.cases = []
        self.serial = 0

    def fixture(self, panes=False):
        self.serial += 1
        session = f"relative{self.serial}"
        self.sessions.append(session)
        first = self.run("new-session", "-d", "-s", session, "-x", "120", "-y", "40",
                         "-P", "-F", "#{window_id}", "cmd.exe /k").stdout.strip()
        windows = [first]
        for index in (1, 2):
            windows.append(self.run("new-window", "-d", "-t", session + ":",
                                    "-n", f"window{index}", "-P", "-F", "#{window_id}",
                                    "cmd.exe /k").stdout.strip())
        if panes:
            for _ in range(2):
                self.run("split-window", "-d", "-t", f"{session}:{first}", "cmd.exe /k")
        return session, windows

    def close_fixture(self, session):
        self.run("kill-session", "-t", session)
        self.sessions.remove(session)

    def focus(self, session, windows, active):
        # Give !/{last} a deterministic, different last-window identity.
        self.run("select-window", "-t", f"{session}:{windows[1 if active != 1 else 2]}")
        self.run("select-window", "-t", f"{session}:{windows[active]}")

    def window_ids(self, session):
        return self.run("list-windows", "-t", session + ":", "-F", "#{window_id}").stdout.splitlines()

    def active(self, session):
        return self.run("display-message", "-p", "-t", session + ":", "#{window_id}").stdout.strip()

    def control(self, session, command):
        stem = self.data / f"{self.ns}__{session}"
        port = int(stem.with_suffix(".port").read_text().strip())
        key = stem.with_suffix(".key").read_text().strip()
        with socket.create_connection(("127.0.0.1", port), timeout=15) as sock:
            peer = sock.makefile("rwb", buffering=0)
            peer.write(f"AUTH {key}\n".encode())
            assert peer.readline() == b"OK\n"
            peer.write(b"CONTROL_NOECHO\n")
            while True:
                line = peer.readline()
                assert line, "control setup closed early"
                if line.startswith(b"%end "):
                    break
            peer.write((command + "\n").encode())
            lines = []
            while True:
                line = peer.readline()
                assert line, "control command closed early"
                lines.append(line)
                if line.startswith((b"%end ", b"%error ")):
                    return b"".join(lines).decode()

    def invoke(self, route, session, command, spec, *args, fail=False):
        target = f"{session}:{spec}"
        if route == "cli":
            p = self.run(command, "-t", target, *args, check=False)
            reply = p.stdout + p.stderr
            success = p.returncode == 0
        elif route == "raw":
            reply = self.wire(session, " ".join((command, *args)), target).decode()
            success = "ERROR:" not in reply
        else:
            reply = self.control(session, " ".join((command, "-t", target, *args)))
            success = "%error " not in reply and "ERROR:" not in reply
        assert success != fail, (route, command, target, args, reply)
        return reply

    def destructive(self, route):
        targets = [("+", 0, 1), ("-", 2, 1), ("+1", 0, 1), ("-1", 2, 1),
                   ("+2", 0, 2), ("-2", 2, 0), ("{next}", 0, 1), ("{previous}", 2, 1),
                   ("{start}", 2, 0), ("{end}", 0, 2), ("!", 0, 1), ("{last}", 0, 1),
                   ("+.+", 0, 1), ("-.-", 2, 1)]
        for command in ("kill-window", "unlink-window"):
            for spec, active, victim in targets:
                session, windows = self.fixture()
                self.focus(session, windows, active)
                self.invoke(route, session, command, spec)
                remaining = self.window_ids(session)
                expected = [w for i, w in enumerate(windows) if i != victim]
                assert remaining == expected, (route, command, spec, expected, remaining)
                self.cases.append(dict(route=route, command=command, target=spec,
                                       victim=windows[victim], remaining=remaining, ok=True))
                self.close_fixture(session)
        print(f"PASS {route}: relative/symbolic kill and unlink", flush=True)

    def attributes_and_selection(self, route):
        session, windows = self.fixture(panes=True)
        for spec, active, destination in [("+", 0, 1), ("-", 2, 1), ("{next}", 0, 1),
                                           ("{previous}", 2, 1), ("!", 0, 1),
                                           ("{start}", 2, 0), ("{end}", 0, 2),
                                           ("+.+", 0, 1), ("-.-", 2, 1)]:
            self.focus(session, windows, active)
            token = f"r{len(self.cases)}"
            self.invoke(route, session, "rename-window", spec, token)
            names = self.run("list-windows", "-t", session + ":", "-F", "#{window_id}|#{window_name}").stdout
            assert f"{windows[destination]}|{token}" in names, (spec, names)
            assert self.active(session) == windows[active]
            self.invoke(route, session, "set-window-option", spec, "@relative-probe", token)
            stored = [self.run("show-window-options", "-v", "-t", f"{session}:{w}",
                               "@relative-probe", check=False).stdout.strip() for w in windows]
            assert stored[destination] == token and stored.count(token) == 1, (spec, stored)
            reply = self.invoke(route, session, "show-window-options", spec, "-v", "@relative-probe")
            assert token in reply, (spec, reply)
            self.invoke(route, session, "select-window", spec)
            assert self.active(session) == windows[destination], (route, spec, self.active(session))
            self.cases.append(dict(route=route, command="rename/options/select", target=spec, ok=True))

        # A relative pane operation must advance once, without a target-focus
        # wrapper advancing it first. Repeat with both CLI argument and wire paths.
        self.focus(session, windows, 0)
        panes = self.run("list-panes", "-t", f"{session}:{windows[0]}", "-F", "#{pane_id}").stdout.splitlines()
        assert len(panes) == 3
        for spec, start, wanted in [(".+", 0, 1), (".-", 2, 1)]:
            self.run("select-pane", "-t", f"{session}:{panes[start]}")
            self.invoke(route, session, "select-pane", spec)
            actual = self.run("display-message", "-p", "-t", session + ":", "#{pane_id}").stdout.strip()
            assert actual == panes[wanted], (route, spec, actual, panes)
            self.cases.append(dict(route=route, command="select-pane", target=spec, ok=True))
            # kill-pane has no relative-pane resolver. It must reject this
            # syntax, rather than drop +/- and kill the currently active pane.
            before = self.run("list-panes", "-s", "-t", session + ":", "-F",
                              "#{window_id}|#{pane_id}|#{pane_pid}").stdout
            self.invoke(route, session, "kill-pane", spec, fail=True)
            after = self.run("list-panes", "-s", "-t", session + ":", "-F",
                             "#{window_id}|#{pane_id}|#{pane_pid}").stdout
            assert before == after, (route, spec, before, after)
            self.cases.append(dict(route=route, command="kill-pane rejects unsupported relative pane", target=spec, ok=True))
        self.close_fixture(session)
        print(f"PASS {route}: rename/options/select and relative panes", flush=True)

    def movers(self, route):
        session, windows = self.fixture()
        self.focus(session, windows, 0)
        self.invoke(route, session, "swap-window", "+", "-d")
        assert self.window_ids(session) == [windows[1], windows[0], windows[2]]
        self.cases.append(dict(route=route, command="swap-window", target="+", ok=True))
        self.close_fixture(session)

        session, windows = self.fixture()
        self.focus(session, windows, 0)
        def pane_in(window):
            return self.run("list-panes", "-t", f"{session}:{window}", "-F", "#{pane_id}").stdout.strip()
        first, second, third = [pane_in(w) for w in windows]
        self.invoke(route, session, "swap-pane", f"{windows[1]}.{second}", "-d")
        assert [pane_in(w) for w in windows] == [second, first, third]
        self.cases.append(dict(route=route, command="swap-pane preserves implicit source", target="@id.%id", ok=True))
        self.close_fixture(session)

        session, windows = self.fixture()
        self.focus(session, windows, 0)
        # A relative destination index is calculated against the original
        # active window, while -s keeps a different explicit source.
        # Upstream has no control-mode move-window dispatcher. Pin the
        # explicit failure/no mutation there instead of treating it as a pass.
        self.invoke(route, session, "move-window", "+3", "-d", "-s", f"{session}:{windows[1]}", fail=route == "control")
        expected = windows if route == "control" else [windows[0], windows[2], windows[1]]
        assert self.window_ids(session) == expected
        assert self.active(session) == windows[0]
        self.cases.append(dict(route=route, command="move-window", target="+3", ok=True))
        self.close_fixture(session)
        print(f"PASS {route}: swap and move keep original source/focus", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--artifacts", default=None)
    args = parser.parse_args()
    before = default_registry_snapshot()
    test = RelativeTargets(args.binary, args.artifacts or tempfile.mkdtemp(prefix="relative-target-results-"))
    error = None
    try:
        for route in ("cli", "raw", "control"):
            test.destructive(route)
            test.attributes_and_selection(route)
            test.movers(route)
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
    print(f"Passed {len(test.cases)} routing cases; default registry unchanged; private registry empty", flush=True)


if __name__ == "__main__":
    main()
