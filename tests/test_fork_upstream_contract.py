"""Focused Windows fork contract checks in a private data dir and unique -L.

Run with --binary pointing to this worktree's built psmux.exe. This never
installs/copies that binary, never invokes kill-server, and never sends a
command to the live/default server. The live registry is snapshotted read-only.
All polling lives inside this single bounded test process.
"""

import argparse
import base64
import collections
import concurrent.futures
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
import uuid


def wait_for(predicate, seconds=10):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("bounded condition did not become true")


def default_registry_snapshot():
    # Do not run even a read-only command against the live server. This is
    # a list of default registry entries and their stable identity anchors.
    data = Path(os.environ.get("PSMUX_DATA_DIR", str(Path.home() / ".psmux")))
    return {
        p.stem: {
            ext: p.with_suffix(ext).read_text(encoding="utf-8").strip()
            for ext in (".port", ".pid", ".sid")
            if p.with_suffix(ext).exists()
        }
        for p in sorted(data.glob("*.port"))
        if "__" not in p.stem
    }


class Contract:
    def __init__(self, binary, artifacts):
        self.binary = str(Path(binary).resolve(strict=True))
        self.artifacts = Path(artifacts).resolve()
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="psmux-fork-contract-"))
        self.data = self.root / "data"
        self.data.mkdir()
        self.ns = f"forkreview_{os.getpid()}_{uuid.uuid4().hex[:8]}"
        self.env = {
            k: v for k, v in os.environ.items()
            if not k.upper().startswith("PSMUX_") and k.upper() not in ("TMUX", "TMUX_PANE")
        }
        config = self.root / "empty.conf"
        config.write_text("", encoding="utf-8")
        self.env.update(PSMUX_DATA_DIR=str(self.data), PSMUX_CONFIG_FILE=str(config),
                        PSMUX_NO_WARM="1", PSMUX_TEST_STALL_HOOK="1")
        self.sessions = []
        self.logs = {}
        self.stops = []
        self.expected = collections.defaultdict(collections.Counter)
        self.results = []
        self.commands = []

    def run(self, *args, check=True):
        assert "kill-server" not in args and "respawn-pane" not in args
        cmd = [self.binary, "-L", self.ns, *args]
        p = subprocess.run(cmd, env=self.env, capture_output=True, encoding="utf-8",
                           errors="replace", timeout=20)
        self.commands.append({"args": list(args), "exit": p.returncode,
                              "out": p.stdout, "err": p.stderr})
        if check:
            assert p.returncode == 0, (args, p.returncode, p.stdout, p.stderr)
        return p

    def wire(self, session, command, target=None, drain=True):
        stem = self.data / f"{self.ns}__{session}"
        port = int(stem.with_suffix(".port").read_text().strip())
        key = stem.with_suffix(".key").read_text().strip()
        with socket.create_connection(("127.0.0.1", port), timeout=12) as sock:
            payload = f"AUTH {key}\n" + (f"TARGET {target}\n" if target else "") + command + "\n"
            sock.sendall(payload.encode("utf-8"))
            if not drain:
                # Match AgentLauncher: LF AUTH/TARGET/command, flush and close,
                # without consuming the acknowledgement or half-closing first.
                return b""
            chunks = []
            while True:
                part = sock.recv(65536)
                if not part:
                    return b"".join(chunks)
                chunks.append(part)

    def probe_args(self, label):
        log = self.root / f"{label}.bin"
        stop = self.root / f"{label}.stop"
        self.logs[label] = log
        self.stops.append(stop)
        return [str(self.probe), str(log), str(stop)]

    def bytes(self, label):
        try:
            return self.logs[label].read_bytes()
        except FileNotFoundError:
            return b""

    def tokens(self, label):
        return collections.Counter(re.findall(rb"\{[A-Z0-9_]+\}", self.bytes(label)))

    def expect(self, label, token, count=1):
        self.expected[label][token.encode()] += count

    def check_tokens(self):
        wait_for(lambda: all(self.tokens(k) == self.expected[k] for k in self.logs), 12)
        for label in self.logs:
            assert self.tokens(label) == self.expected[label], (label, self.bytes(label), self.expected[label])

    def step(self, name, action):
        started = time.monotonic()
        action()
        self.results.append({"test": name, "ok": True, "seconds": round(time.monotonic()-started, 3)})
        print(f"PASS {name}", flush=True)

    def setup(self):
        csc = Path(os.environ["WINDIR"]) / "Microsoft.NET/Framework64/v4.0.30319/csc.exe"
        source = Path(__file__).with_name("target_routing_probe.cs")
        self.probe = self.root / "target_routing_probe.exe"
        p = subprocess.run([str(csc), "/nologo", "/optimize", "/platform:x64",
                            f"/out:{self.probe}", str(source)], capture_output=True, text=True, timeout=30)
        assert p.returncode == 0, (p.stdout, p.stderr)
        for session, label in (("alpha", "A"), ("beta", "Z")):
            self.sessions.append(session)
            pane = self.run("new-session", "-d", "-s", session, "-x", "120", "-y", "30",
                            "-P", "-F", "#{pane_id}", "--", *self.probe_args(label)).stdout.strip()
            setattr(self, label, f"{session}:{pane}")
        assert self.A.split(":")[-1] == self.Z.split(":")[-1], "precondition: ids collide between sessions"
        pane = self.run("split-window", "-d", "-t", self.A, "-P", "-F", "#{pane_id}",
                        "--", *self.probe_args("B")).stdout.strip()
        self.B = f"alpha:{pane}"
        pane = self.run("new-window", "-d", "-t", "alpha:", "-P", "-F", "#{pane_id}",
                        "--", *self.probe_args("C")).stdout.strip()
        self.C = f"alpha:{pane}"
        for target in (self.A, self.B, self.C, self.Z):
            wait_for(lambda t=target: "target_routing_probe" in self.run("capture-pane", "-e", "-p", "-t", t).stdout)

    def defaults_and_identity(self):
        assert self.run("show-options", "-s", "-v", "-t", "alpha:", "escape-time").stdout.strip() == "500"
        assert self.run("show-options", "-s", "-v", "-t", "alpha:", "set-clipboard").stdout.strip() == "on"
        assert self.run("show-options", "-s", "-v", "-t", "alpha:", "server-mode").stdout.strip() == "per-session"
        # PSMUX_NO_WARM disables runtime spares for isolation; compiled depth
        # and the catalog's reset value are covered by focused unit tests.
        self.run("send-keys", "-t", self.Z, "-l", "{Z_ONLY}")
        self.expect("Z", "{Z_ONLY}")
        self.run("send-keys", "-t", self.A, "-l", "{A_ONLY}")
        self.expect("A", "{A_ONLY}")
        self.nudge()
        self.check_tokens()
        p = self.run("display-message", "-p", "-t", self.B, "ERROR: ordinary pane data")
        assert p.stdout.strip() == "ERROR: ordinary pane data"

    def nudge(self):
        for label, target in (("A", self.A), ("B", self.B), ("C", self.C), ("Z", self.Z)):
            before = len(self.bytes(label))
            self.run("send-keys", "-t", target, "-l", ".")
            wait_for(lambda k=label, n=before: len(self.bytes(k)) > n)

    def races(self):
        before = self.run("display-message", "-p", "-t", "alpha:", "#{window_id}.#{pane_id}").stdout
        for index, stall in enumerate((0, 0, 0, 0, 1500, 6200)):
            if stall:
                self.wire("alpha", f"debug-stall {stall}")
                time.sleep(0.1)
            tasks = []
            for label, target in (("A", self.A), ("B", self.B), ("C", self.C), ("Z", self.Z)):
                token = f"{{{label}{index}}}"
                tasks.append(("send-keys", "-t", target, "-l", token))
                self.expect(label, token)
            repeated = f"{{N{index}}}"
            tasks.append(("send-keys", "-N", "3", "-t", self.B, "-l", repeated))
            self.expect("B", repeated, 3)
            for _ in range(3):
                tasks.append(("capture-pane", "-e", "-p", "-t", self.C))
                tasks.append(("list-panes", "-s", "-t", "alpha:", "-F", "#{window_id}.#{pane_id}"))
            with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
                outcomes = list(pool.map(lambda a: self.run(*a, check=stall < 3000), tasks))
            self.nudge()
            self.check_tokens()
            self.results.append({"race": index, "stall_ms": stall,
                                 "nonzero_cli_exits": sum(p.returncode != 0 for p in outcomes)})
        after = self.run("display-message", "-p", "-t", "alpha:", "#{window_id}.#{pane_id}").stdout
        assert before == after, (before, after)

    def raw_agent_wire(self):
        encoded_a = base64.b64encode(b"{RAW_A}").decode("ascii")
        encoded_z = base64.b64encode(b"{RAW_Z}").decode("ascii")
        for label, target, command in (("A", self.A, f"send-paste {encoded_a}"),
                                       ("B", self.B, "send-keys -l {RAW_B}"),
                                       ("Z", self.Z, f"send-paste {encoded_z}")):
            self.wire("beta" if label == "Z" else "alpha", command, target, drain=False)
            self.expect(label, f"{{RAW_{label}}}")
        self.nudge()
        self.check_tokens()
        for target in (self.A, self.B, self.C, self.Z):
            assert "target_routing_probe" in self.run("capture-pane", "-e", "-p", "-t", target).stdout

    def aliases(self):
        for key, expected in (("Enter", b"\r"), ("Return", b"\r"), ("CR", b"\r"), ("Escape", b"\x1b")):
            before = len(self.bytes("B"))
            self.run("send-keys", "-t", self.B, key)
            self.run("send-keys", "-t", self.B, "-l", ".")
            wait_for(lambda: len(self.bytes("B")) >= before + len(expected) + 1)
            assert self.bytes("B")[before:] == expected + b".", (key, self.bytes("B")[before:])

    def vanished_target(self):
        pane = self.run("split-window", "-d", "-t", self.A, "-P", "-F", "#{pane_id}",
                        "--", *self.probe_args("D")).stdout.strip()
        target = f"alpha:{pane}"
        wait_for(lambda: "target_routing_probe" in self.run("capture-pane", "-e", "-p", "-t", target).stdout)
        self.run("kill-pane", "-t", target)
        for command in (("send-keys", "-t", target, "-l", "{VANISHED}"),
                        ("send-keys", "-t", "alpha:%9999999", "-l", "{MISSING}")):
            p = self.run(*command, check=False)
            assert p.returncode != 0 and "pane" in p.stderr + p.stdout, (p.stdout, p.stderr)
        self.nudge()
        self.check_tokens()
        assert all(b"{VANISHED}" not in self.bytes(label) and b"{MISSING}" not in self.bytes(label) for label in self.logs)

    def open_pr7_ordering_probe(self):
        # Regression for the separate PR's pane-to-window mapping. Both
        # possible victims are disposable probes; never target a live session.
        self.sessions.append("gamma")
        first = self.run("new-session", "-d", "-s", "gamma", "-P", "-F", "#{pane_id}",
                         "--", *self.probe_args("G")).stdout.strip()
        second = self.run("new-window", "-d", "-t", "gamma:", "-P", "-F", "#{pane_id}",
                          "--", *self.probe_args("H")).stdout.strip()
        target = f"gamma:{second}"
        self.run("kill-window", "-t", target)
        remaining = self.run("list-panes", "-s", "-t", "gamma:", "-F", "#{pane_id}").stdout.splitlines()
        fixed = first in remaining and second not in remaining
        self.results.append({"open_pr7_window_target_fixed": fixed, "requested_pane": second,
                             "active_pane_before": first, "remaining_panes": remaining,
                             "requires_pr7_rebase": not fixed})
        print(f"PR7 DIAGNOSTIC: target-safe={fixed}; requested {second}; remaining {remaining}", flush=True)
        assert fixed, "pane-to-window safety from PR #7 must be present"

    def cleanup(self):
        for stop in self.stops:
            stop.touch()
        for session in reversed(self.sessions):
            self.run("kill-session", "-t", session, check=False)
        wait_for(lambda: not list(self.data.glob(f"{self.ns}__*.port")), 12)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--artifacts", required=True)
    parser.add_argument("--probe-open-pr7", action="store_true",
                        help="Only diagnose the separate open PR #7 window-target issue")
    args = parser.parse_args()
    before = default_registry_snapshot()
    test = Contract(args.binary, args.artifacts)
    error = None
    try:
        test.step("private namespace setup; colliding ids in two sessions", test.setup)
        if args.probe_open_pr7:
            test.open_pr7_ordering_probe()
        else:
            test.step("fork defaults, qualified targets and ERROR data", test.defaults_and_identity)
            test.step("concurrent send/capture/list, repeats, short and 6.2s stalled dispatch", test.races)
            test.step("AgentLauncher LF AUTH/TARGET immediate-close wire and styled capture", test.raw_agent_wire)
            test.step("Enter/Return/CR/Escape bytes", test.aliases)
            test.step("disappearing and invalid targets never redirect", test.vanished_target)
    except BaseException as exc:
        error = repr(exc)
        raise
    finally:
        try:
            test.cleanup()
        finally:
            after = default_registry_snapshot()
            summary = {"namespace": test.ns, "private_root": str(test.root), "binary": test.binary,
                       "error": error, "results": test.results, "default_registry_before": before,
                       "default_registry_after": after, "default_registry_unchanged": before == after,
                       "private_session_registry_empty": not list(test.data.glob(f"{test.ns}__*.port")),
                       "probe_logs": {k: test.bytes(k).hex() for k in test.logs}, "commands": test.commands}
            (test.artifacts / "contract.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
            assert before == after, "default registry changed: investigate before further runtime checks"


if __name__ == "__main__":
    main()
