"""Getopt spelling equivalence, including input delivery, in private -L servers."""
import argparse
import json
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
from test_relative_target_safety import RelativeTargets
from test_fork_upstream_contract import default_registry_snapshot, wait_for


class Attached(RelativeTargets):
    def invoke_args(self, route, session, args, fail=False):
        assert session in self.sessions
        if route == "cli":
            # Respawn rejection probes are restricted to a named owned fixture.
            if args[0] in ("respawn-pane", "respawn-window"):
                assert any(session + ":%bad" in arg or arg.startswith("--target-pane=" + session + ":") for arg in args)
                p = subprocess.run([self.binary, "-L", self.ns, *args], env=self.env,
                                   capture_output=True, text=True, timeout=20)
            else:
                p = self.run(*args, check=False)
            success, reply = p.returncode == 0, p.stdout + p.stderr
        else:
            command = " ".join(json.dumps(arg) if any(c.isspace() for c in arg) else arg for arg in args)
            reply = self.wire(session, command).decode() if route == "raw" else self.control(session, command)
            success = "ERROR:" not in reply and "%error " not in reply
        assert success != fail, (route, args, reply)
        return reply

    def check_delivery(self, route, args, token):
        before = {label: self.bytes(label) for label in self.logs}
        self.invoke_args(route, "alpha", args)
        wait_for(lambda: token.encode() in self.bytes("B")[len(before["B"]):])
        assert all(token.encode() not in self.bytes(label)[len(data):] for label, data in before.items() if label != "B")
        self.cases.append(dict(route=route, command=args[0], spelling=args[1:], ok=True))

    def delivery(self, route):
        forms = [("separate", ["-t", self.B]), ("attached", ["-t" + self.B]),
                 ("equals", ["-t=" + self.B]), ("long", ["--target", self.B]),
                 ("long-equals", ["--target=" + self.B])]
        for label, target in forms:
            token = "{GETOPT_SEND_" + route + "_" + label.replace('-', '_') + "}"
            self.check_delivery(route, ["send-keys", *target, "-l", token], token)
            token = "{GETOPT_PASTE_" + route + "_" + label.replace('-', '_') + "}"
            self.wire("alpha", "set-buffer -b attached " + token)
            self.check_delivery(route, ["paste-buffer", *target, "-b", "attached"], token)
        token = "{GETOPT_CLUSTER_SEND_" + route + "}"
        self.check_delivery(route, ["send-keys", "-lt" + self.B, token], token)
        token = "{GETOPT_CLUSTER_PASTE_" + route + "}"
        self.wire("alpha", "set-buffer -b attached " + token)
        self.check_delivery(route, ["paste-buffer", "-dt" + self.B, "-b", "attached"], token)
        token = "-talpha:%1_GETOPT_DATA_" + route
        self.check_delivery(route, ["send-keys", "-t" + self.B, "-l", "--", token], token)
        self.invoke_args(route, "alpha", ["set-option", "-pt" + self.B, "@getopt-data", "-w"])
        assert self.run("display-message", "-p", "-t", self.B, "#{@getopt-data}").stdout.strip() == "-w"
        assert self.run("display-message", "-p", "-t", self.A, "#{@getopt-data}").stdout.strip() == ""
        self.cases.append(dict(route=route, command="pane option flag-shaped value", ok=True))
        # A nested/config command must canonicalize before target extraction too.
        token = "{GETOPT_BINDING_" + route + "}"
        command = "send-keys -lt" + self.B + " " + token
        self.wire("alpha", "run-command " + json.dumps(command))
        wait_for(lambda: token.encode() in self.bytes("B"))
        assert all(token.encode() not in self.bytes(label) for label in self.logs if label != "B")
        self.cases.append(dict(route=route, command="in-server attached input", ok=True))
        token = "{GETOPT_BINDING_PASTE_" + route + "}"
        self.wire("alpha", "set-buffer -b attached " + token)
        self.wire("alpha", "run-command " + json.dumps("paste-buffer -t" + self.B + " -b attached"))
        wait_for(lambda: token.encode() in self.bytes("B"))
        assert all(token.encode() not in self.bytes(label) for label in self.logs if label != "B")
        self.cases.append(dict(route=route, command="in-server attached paste", ok=True))
        token = "{GETOPT_NESTED_" + route + "}"
        command = "send-keys -lt" + self.B + " " + token
        if route == "cli":
            self.run("if-shell", "-t", "alpha:", "-F", "1", command)
        else:
            self.invoke_args(route, "alpha", ["if-shell", "-F", "1", command])
        wait_for(lambda: token.encode() in self.bytes("B"))
        assert all(token.encode() not in self.bytes(label) for label in self.logs if label != "B")
        self.cases.append(dict(route=route, command="nested attached input", ok=True))
        print(f"PASS {route}: attached/clustered/long send and paste reach only B", flush=True)

    def state(self, session):
        return self.run("list-panes", "-s", "-t", session + ":", "-F",
                        "#{window_id}|#{window_active}|#{window_layout}|#{pane_id}|#{pane_pid}|"
                        "#{pane_active}|#{pane_width}|#{pane_height}|#{window_zoomed_flag}|#{@getopt-window}|#{@getopt-pane}").stdout

    def mutations(self, route):
        session, windows = self.fixture(panes=True)
        bad = session + ":%bad"
        destination = session + ":" + windows[1]
        pane_destination = destination + ".0"
        commands = []
        for command, cluster in [("kill-window", "at"), ("unlink-window", "t"),
                                 ("kill-pane", "at"), ("respawn-pane", "kt"), ("respawn-window", "kt")]:
            for target in [["-t" + bad], ["-" + cluster + bad], ["-" + cluster, bad],
                           ["--target=" + bad], ["--target", bad], ["--target-pane=" + destination]]:
                commands.append([command, *target])
        for command in ("zoom-pane", "send-key", "send-text", "send-paste"):
            commands.append([command, "--target-pane=" + bad])
            commands.append([command, "-t"])
        for command, prefix, key in [("set-option", "wt", "@getopt-window"),
                                     ("set-option", "wat", "@getopt-window"),
                                     ("set-option", "pt", "@getopt-pane"),
                                     ("set-window-option", "at", "@getopt-window")]:
            for target in [["-" + prefix + bad], ["-" + prefix, bad]]:
                commands.append([command, *target, key, "WRONG"])
        for command in ("swap-window", "move-window", "swap-pane", "join-pane"):
            dest = session + ":9" if command == "move-window" else pane_destination if command.endswith('pane') else destination
            for source in [["-ds" + bad], ["-ds", bad], ["-d", "--source=" + bad]]:
                commands.append([command, *source, "-t", dest])
        for args in commands:
            before = self.state(session)
            self.invoke_args(route, session, args, fail=True)
            assert self.state(session) == before, (route, args, "state changed after rejection")
            self.cases.append(dict(route=route, args=args, unchanged=True, ok=True))
        source_pane = self.run("list-panes", "-t", destination, "-F", "#{pane_id}").stdout.strip()
        destination_pane = self.run("list-panes", "-t", f"{session}:{windows[2]}", "-F", "#{pane_id}").stdout.strip()
        for spelling in ("clustered", "long"):
            flags = ["-ds" + session + ":" + source_pane, "-t" + session + ":" + destination_pane] if spelling == "clustered" else ["-d", "--source=" + session + ":" + source_pane, "--target=" + session + ":" + destination_pane]
            self.invoke_args(route, session, ["swap-window", *flags])
            expected = [windows[0], windows[2], windows[1]] if spelling == "clustered" else windows
            assert self.window_ids(session) == expected
            # Baseline and tmux select the destination slot WITH -d, which
            # now contains the original source window. An unrelated active
            # window is not an oracle for swap-window -d.
            assert self.active(session) == windows[1]
            self.cases.append(dict(route=route, command="swap-window", spelling=spelling, source_and_baseline_focus=True, ok=True))
        self.run("select-window", "-t", f"{session}:{windows[0]}")
        ids = self.run("list-panes", "-t", f"{session}:{windows[0]}", "-F", "#{pane_id}").stdout.splitlines()
        self.invoke_args(route, session, ["swap-pane", "-ds" + session + ":" + ids[1], "-t" + session + ":" + ids[2]])
        assert self.run("list-panes", "-t", f"{session}:{windows[0]}", "-F", "#{pane_id}").stdout.splitlines() == [ids[0], ids[2], ids[1]]
        self.cases.append(dict(route=route, command="swap-pane", spelling="clustered source and attached target", ok=True))
        self.invoke_args(route, session, ["swap-pane", "-d", "--source=" + session + ":" + ids[1], "--target=" + session + ":" + ids[2]])
        assert self.run("list-panes", "-t", f"{session}:{windows[0]}", "-F", "#{pane_id}").stdout.splitlines() == ids
        self.cases.append(dict(route=route, command="swap-pane", spelling="long source and target", ok=True))
        self.invoke_args(route, session, ["set-option", "-wt" + destination, "@getopt-window", "RIGHT"])
        values = [self.run("show-window-options", "-v", "-t", f"{session}:{window}", "@getopt-window", check=False).stdout.strip() for window in windows]
        assert values[1] == "RIGHT" and values.count("RIGHT") == 1
        self.cases.append(dict(route=route, command="set-option", spelling="attached window scope", ok=True))
        self.wire(session, "run-command " + json.dumps("set-option -wt" + destination + " @getopt-window LOCAL"))
        wait_for(lambda: self.run("show-window-options", "-v", "-t", destination, "@getopt-window").stdout.strip() == "LOCAL")
        self.cases.append(dict(route=route, command="in-server attached window option", ok=True))
        before = self.state(session)
        self.wire(session, "run-command " + json.dumps("set-option -wt" + bad + " @getopt-window WRONG"))
        assert self.state(session) == before
        self.cases.append(dict(route=route, command="in-server malformed window option", unchanged=True, ok=True))
        self.close_fixture(session)
        # Concrete valid attached targets must remove the non-active window.
        for spelling in ("attached", "separate", "long"):
            session, windows = self.fixture()
            pane = self.run("list-panes", "-t", f"{session}:{windows[1]}", "-F", "#{pane_id}").stdout.strip()
            target = f"{session}:{pane}"
            flags = ["-t" + target] if spelling == "attached" else ["-t", target] if spelling == "separate" else ["--target=" + target]
            self.invoke_args(route, session, ["kill-window", *flags])
            assert self.window_ids(session) == [windows[0], windows[2]]
            self.cases.append(dict(route=route, command="kill-window", spelling=spelling, correct_victim=True, ok=True))
            self.close_fixture(session)
        print(f"PASS {route}: malformed primary/source flags preserve state; valid target kills its owner", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--artifacts", default=None)
    args = parser.parse_args()
    test = Attached(args.binary, args.artifacts or tempfile.mkdtemp(prefix="getopt-results-"))
    before = default_registry_snapshot()
    error = None
    try:
        test.setup()
        for route in ("cli", "raw", "control"):
            test.delivery(route)
            test.mutations(route)
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
            assert before == after
    print(f"Passed {len(test.cases)} getopt-routing cases; default registry unchanged; private registry empty", flush=True)


if __name__ == "__main__":
    main()
