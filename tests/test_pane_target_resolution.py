"""Supported pane selectors resolve once to identity in private -L fixtures.

The explicit worktree binary runs only with unique namespaces/private config
and registries. Valid respawn/kill probes affect only owned disposable panes.
No install, binary copy, kill-server or live/default-server command is used.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
from test_relative_target_safety import RelativeTargets
from test_fork_upstream_contract import default_registry_snapshot, wait_for


SELECTORS = [(".+", 0, 1), (".-", 2, 1), (".+2", 0, 2), (".-2", 2, 0),
             (".{next}", 0, 1), (".{previous}", 2, 1), (".!", 0, 1),
             (".{last}", 2, 1), (".{top-left}", 2, 0), (".{bottom-left}", 0, 2),
             (".{down-of}", 0, 1), (".{up-of}", 2, 1)]


class PaneOps(RelativeTargets):
    def run(self, *args, check=True):
        if args and args[0] == "respawn-pane":
            assert "-t" in args
            target = args[args.index("-t") + 1]
            assert target.split(':', 1)[0] in self.sessions, "respawn must name an owned session"
            p = subprocess.run([self.binary, "-L", self.ns, *args], env=self.env, capture_output=True,
                               encoding="utf-8", errors="replace", timeout=20)
            self.commands.append(dict(args=list(args), exit=p.returncode, out=p.stdout, err=p.stderr))
            if check:
                assert p.returncode == 0, (args, p.stdout, p.stderr)
            return p
        return super().run(*args, check=check)

    def panes(self, session, window=None):
        args = ["list-panes", "-t", f"{session}:{window or ''}"]
        if window is None:
            args.append("-s")
        rows = self.run(*args, "-F", "#{pane_id}|#{pane_pid}|#{window_id}|#{pane_height}").stdout.splitlines()
        return [line.split('|') for line in rows if line.startswith('%')]

    def focus_pane(self, session, ids, start):
        self.run("select-pane", "-t", f"{session}:{ids[1]}")
        self.run("select-pane", "-t", f"{session}:{ids[start]}")
        wait_for(lambda: self.run("display-message", "-p", "-t", session + ":", "#{pane_id}").stdout.strip() == ids[start])

    def record(self, route, command, target):
        self.cases.append(dict(route=route, command=command, target=target, ok=True))

    def respawn_capture_pipe_resize(self, route):
        session, windows = self.fixture(panes=True)
        ids = [row[0] for row in self.panes(session, windows[0])]
        assert len(ids) == 3
        for spec, start, wanted in SELECTORS:
            self.focus_pane(session, ids, start)
            before = {row[0]: row[1] for row in self.panes(session)}
            self.invoke(route, session, "respawn-pane", spec, "-k", "cmd.exe /k")
            wait_for(lambda: dict((row[0], row[1]) for row in self.panes(session))[ids[wanted]] != before[ids[wanted]])
            after = {row[0]: row[1] for row in self.panes(session)}
            assert [pane for pane in before if before[pane] != after[pane]] == [ids[wanted]], (route, spec, before, after)
            assert self.run("display-message", "-p", "-t", session + ":", "#{pane_id}").stdout.strip() == ids[start]
            self.record(route, "respawn-pane changes only resolved process", spec)
        for spec, start in [(".+", 0), (".-", 2)]:
            self.focus_pane(session, ids, start)
            markers = [f"CAP_{route}_{index}_{len(self.cases)}" for index in range(3)]
            for pane, marker in zip(ids, markers):
                self.run("send-keys", "-t", f"{session}:{pane}", "-l", "echo " + marker)
                self.run("send-keys", "-t", f"{session}:{pane}", "Enter")
                wait_for(lambda p=pane, m=marker: m in self.run("capture-pane", "-p", "-t", f"{session}:{p}").stdout)
            capture = self.invoke(route, session, "capture-pane", spec, "-p")
            assert markers[1] in capture and all(marker not in capture for index, marker in enumerate(markers) if index != 1)
            self.record(route, "capture-pane reads resolved pane", spec)
            path = self.root / f"pipe_{route}_{start}.txt"
            self.invoke(route, session, "pipe-pane", spec, "cat", ">", path.as_posix())
            pipe_markers = [f"PIPE_{route}_{index}_{len(self.cases)}" for index in range(3)]
            for pane, marker in zip(ids, pipe_markers):
                self.run("send-keys", "-t", f"{session}:{pane}", "-l", "echo " + marker)
                self.run("send-keys", "-t", f"{session}:{pane}", "Enter")
            wait_for(lambda: path.exists() and pipe_markers[1].encode() in path.read_bytes())
            data = path.read_bytes()
            assert all(marker.encode() not in data for index, marker in enumerate(pipe_markers) if index != 1), (route, spec, data)
            self.run("pipe-pane", "-t", f"{session}:{ids[1]}")
            self.record(route, "pipe-pane receives only resolved pane output", spec)
            self.run("select-layout", "-t", f"{session}:{windows[0]}", "even-vertical")
            self.invoke(route, session, "resize-pane", spec, "-y", "7")
            wait_for(lambda: next(row[3] for row in self.panes(session) if row[0] == ids[1]) == "7")
            self.record(route, "resize-pane changes resolved pane size", spec)
        self.close_fixture(session)
        print(f"PASS {route}: respawn, capture, pipe and resize preserve target identity", flush=True)

    def options_input(self, route):
        ids = [row[0] for row in self.panes("alpha", self.first_window)]
        for spec, start, wanted in SELECTORS:
            self.focus_pane("alpha", ids, start)
            token = f"{{RESOLVE_{route}_{len(self.cases)}}}"
            self.invoke(route, "alpha", "set-option", spec, "-p", "@resolve-probe", token)
            values = [self.run("display-message", "-p", "-t", f"alpha:{pane}", "#{@resolve-probe}").stdout.strip() for pane in ids]
            assert values[wanted] == token and values.count(token) == 1, (route, spec, ids, values)
            self.record(route, "pane option writes only resolved pane", spec)
            reply = self.invoke(route, "alpha", "display-message", spec, "-p", "#{pane_id}")
            assert ids[wanted] in reply.splitlines(), (route, spec, reply, ids[wanted])
            self.record(route, "display-message reads resolved pane", spec)
            self.invoke(route, "alpha", "send-keys", spec, "-l", token)
            label = self.labels[ids[wanted]]
            wait_for(lambda: token.encode() in self.bytes(label))
            assert all(token.encode() not in self.bytes(other) for other in self.logs if other != label)
            self.record(route, "send-keys reaches only resolved process", spec)
            self.invoke(route, "alpha", "select-pane", spec, "-T", token)
            titles = self.run("list-panes", "-t", f"alpha:{self.first_window}", "-F", "#{pane_id}|#{pane_title}").stdout.splitlines()
            assert f"{ids[wanted]}|{token}" in titles
            assert self.run("display-message", "-p", "-t", "alpha:", "#{pane_id}").stdout.strip() == ids[start]
            self.record(route, "relative pane attributes preserve real focus", spec)
        # Exact built-in prefix-o command, with context carried by the socket.
        self.focus_pane("alpha", ids, 0)
        self.wire("alpha", "select-pane -t :.+")
        wait_for(lambda: self.run("display-message", "-p", "-t", "alpha:", "#{pane_id}").stdout.strip() == ids[1])
        self.record(route, "built-in prefix-o wire command cycles once", ":.+")
        # The RunCommand path executes bindings/config commands on AppState,
        # rather than going directly through connection dispatch.
        self.focus_pane("alpha", ids, 0)
        self.wire("alpha", "run-command " + json.dumps("select-pane -t :.+"))
        wait_for(lambda: self.run("display-message", "-p", "-t", "alpha:", "#{pane_id}").stdout.strip() == ids[1])
        self.record(route, "in-server binding execution cycles once", ":.+")
        self.focus_pane("alpha", ids, 0)
        branch = "set-option -p -t alpha:.+ @nested-resolved NESTED_" + route
        if route == "cli":
            self.run("if-shell", "-t", "alpha:", "-F", "1", branch)
        elif route == "raw":
            self.wire("alpha", "if-shell -F 1 " + json.dumps(branch))
        else:
            reply = self.control("alpha", "if-shell -F 1 " + json.dumps(branch))
            assert "%error " not in reply, reply
        wait_for(lambda: self.run("display-message", "-p", "-t", f"alpha:{ids[1]}", "#{@nested-resolved}").stdout.strip() == "NESTED_" + route)
        values = [self.run("display-message", "-p", "-t", f"alpha:{pane}", "#{@nested-resolved}").stdout.strip() for pane in ids]
        assert values.count("NESTED_" + route) == 1
        assert self.run("display-message", "-p", "-t", "alpha:", "#{pane_id}").stdout.strip() == ids[0]
        self.record(route, "nested if-shell resolves branch target", ".+")
        print(f"PASS {route}: pane options, display, input, attributes and prefix-o", flush=True)

    def structural(self, route):
        for command in ("kill-pane", "join-pane", "move-pane", "break-pane"):
            for spec, start in [(".+", 0), (".-", 2)]:
                session, windows = self.fixture(panes=True)
                ids = [row[0] for row in self.panes(session, windows[0])]
                self.focus_pane(session, ids, start)
                before = {row[0]: (row[1], row[2]) for row in self.panes(session)}
                if command == "kill-pane":
                    self.invoke(route, session, command, spec)
                    wait_for(lambda: ids[1] not in [row[0] for row in self.panes(session)])
                else:
                    destination = "9" if command == "break-pane" else windows[1] + ".0"
                    self.invoke(route, session, command, destination, "-s", f"{session}:{spec}", "-d")
                    wait_for(lambda: next(row[2] for row in self.panes(session) if row[0] == ids[1]) != windows[0])
                after = {row[0]: (row[1], row[2]) for row in self.panes(session)}
                for pane, identity in before.items():
                    if pane != ids[1]:
                        assert after[pane] == identity, (route, command, spec, pane, before, after)
                if command != "kill-pane":
                    assert set(before) == set(after) and before[ids[1]][0] == after[ids[1]][0]
                    if command != "break-pane":
                        assert after[ids[1]][1] == windows[1]
                self.record(route, command + " consumes resolved source/target", spec)
                self.close_fixture(session)
        session, windows = self.fixture(panes=True)
        ids = [row[0] for row in self.panes(session, windows[0])]
        self.focus_pane(session, ids, 0)
        self.invoke(route, session, "swap-pane", ".+2", "-s", f"{session}:.+", "-d")
        wait_for(lambda: [row[0] for row in self.panes(session, windows[0])] == [ids[0], ids[2], ids[1]])
        self.record(route, "swap-pane resolves both relative ends together", ".+ / .+2")
        self.close_fixture(session)
        # Default untargeted swap and rotate bindings retain their semantics.
        session, windows = self.fixture(panes=True)
        ids = [row[0] for row in self.panes(session, windows[0])]
        self.focus_pane(session, ids, 1)
        self.wire(session, "swap-pane -U")
        wait_for(lambda: [row[0] for row in self.panes(session, windows[0])] == [ids[1], ids[0], ids[2]])
        self.wire(session, "swap-pane -D")
        wait_for(lambda: [row[0] for row in self.panes(session, windows[0])] == ids)
        self.wire(session, "rotate-window -U")
        wait_for(lambda: [row[0] for row in self.panes(session, windows[0])] != ids)
        self.wire(session, "rotate-window -D")
        wait_for(lambda: [row[0] for row in self.panes(session, windows[0])] == ids)
        self.record(route, "default swap/rotate bindings remain inverse operations", "-U / -D")
        self.close_fixture(session)
        print(f"PASS {route}: kill/join/move/break/swap and default navigation bindings", flush=True)

    def cross_session(self):
        for command in ("join-pane", "move-pane"):
            source, source_windows = self.fixture(panes=True)
            target, target_windows = self.fixture(panes=True)
            source_ids = [row[0] for row in self.panes(source, source_windows[0])]
            target_ids = [row[0] for row in self.panes(target, target_windows[0])]
            self.focus_pane(source, source_ids, 0)
            self.focus_pane(target, target_ids, 0)
            initial_source = self.panes(source)
            initial_target = self.panes(target)
            for bad in (".%bad", ".{not-a-pane}"):
                result = self.run(command, "-s", f"{source}:.+", "-t", f"{target}:{bad}", "-d", check=False)
                assert result.returncode != 0
                assert self.panes(source) == initial_source and self.panes(target) == initial_target
                self.record("cli", command + " refuses invalid destination before extraction", bad)
            moved_pid = next(row[1] for row in initial_source if row[0] == source_ids[1])
            self.run(command, "-s", f"{source}:.+", "-t", f"{target}:.+", "-d")
            wait_for(lambda: source_ids[1] not in [row[0] for row in self.panes(source)])
            wait_for(lambda: moved_pid in [row[1] for row in self.panes(target, target_windows[0])])
            assert [row[:3] for row in self.panes(source)] == [row[:3] for row in initial_source if row[0] != source_ids[1]]
            for row in initial_target:
                assert row[:3] in [item[:3] for item in self.panes(target)]
            target_pids = [row[1] for row in self.panes(target, target_windows[0])]
            expected = [row[1] for row in initial_target if row[2] == target_windows[0]]
            expected.insert(2, moved_pid)
            assert target_pids == expected, (command, target_pids, expected)
            self.record("cli", command + " resolves cross-session endpoints before extraction", ".+ / .+")
            self.close_fixture(target)
            self.close_fixture(source)
        print("PASS cli: cross-session relative sources and destinations preserve process identity", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--artifacts", default=None)
    args = parser.parse_args()
    before = default_registry_snapshot()
    test = PaneOps(args.binary, args.artifacts or tempfile.mkdtemp(prefix="pane-resolve-results-"))
    error = None
    try:
        test.setup()
        extra = test.run("split-window", "-d", "-t", test.A, "-P", "-F", "#{pane_id}", "--", *test.probe_args("E")).stdout.strip()
        wait_for(lambda: "target_routing_probe" in test.run("capture-pane", "-p", "-t", f"alpha:{extra}").stdout)
        test.first_window = test.run("display-message", "-p", "-t", test.A, "#{window_id}").stdout.strip()
        test.labels = {test.A.split(':')[-1]: "A", test.B.split(':')[-1]: "B", extra: "E", test.C.split(':')[-1]: "C"}
        for route in ("cli", "raw", "control"):
            test.respawn_capture_pipe_resize(route)
            test.options_input(route)
            test.structural(route)
        test.cross_session()
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
    print(f"Passed {len(test.cases)} supported-selector cases; default registry unchanged; private registry empty", flush=True)


if __name__ == "__main__":
    main()
