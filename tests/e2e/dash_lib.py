#!/usr/bin/env python3
"""In-fork herdr native-dash E2E harness.

Drives the REAL release binary (no mocks): pre-spawns the herdr server headless,
attaches a pyte pty client, and reconstructs the rendered grid the same way a
terminal would. Uses the real $HOME so the usage strip reads real
~/.claude / ~/.codex / ~/.pi data and the goal panel reads real ~/.claude/goal
stores; herdr's own config/state/server socket are isolated under a short
/tmp XDG root (macOS sun_path length limit).

Agent focus is driven through `herdr pane report-agent`, which is the exact
socket protocol the shipped claude/codex/pi hook integrations call — not a mock.
"""
import os, sys, pty, time, fcntl, struct, termios, tempfile, select, shutil, subprocess, json

import pyte

BIN = os.path.expanduser("~/Projects/herdr/target/release/herdr")
REAL_HOME = os.path.expanduser("~")


class Herdr:
    def __init__(self, rows=44, cols=170, usage_strip=True, goal_panel=False,
                 session=None, extra_config=""):
        self.rows, self.cols = rows, cols
        self.session = session or ("e%d" % (os.getpid() % 100000))
        self.root = tempfile.mkdtemp(prefix="hd-", dir="/tmp")
        cfg = os.path.join(self.root, "config", "herdr")
        os.makedirs(cfg, exist_ok=True)
        conf = "[ui]\nusage_strip = %s\ngoal_panel = %s\n%s" % (
            str(usage_strip).lower(), str(goal_panel).lower(), extra_config)
        open(os.path.join(cfg, "config.toml"), "w").write(conf)
        self.env = dict(os.environ, HOME=REAL_HOME,
                        XDG_CONFIG_HOME=os.path.join(self.root, "config"),
                        XDG_STATE_HOME=os.path.join(self.root, "state"),
                        XDG_DATA_HOME=os.path.join(self.root, "data"),
                        TERM="xterm-256color")
        for k in ("HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH"):
            self.env.pop(k, None)
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.fd = None
        self.pid = None
        self._spawner = None

    def cli(self, *args, timeout=20):
        return subprocess.run([BIN, "--session", self.session, *args],
                              env=self.env, capture_output=True, text=True, timeout=timeout)

    def _wait_server(self, secs=25):
        for _ in range(secs * 2):
            try:
                if "status: running" in self.cli("status", "server").stdout:
                    return True
            except subprocess.TimeoutExpired:
                pass
            time.sleep(0.5)
        return False

    def start(self):
        self._spawner = subprocess.Popen([BIN, "--session", self.session], env=self.env,
                                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                         stderr=subprocess.DEVNULL)
        if not self._wait_server():
            raise RuntimeError("server never became ready")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ.clear()
            os.environ.update(self.env)
            os.execv(BIN, [BIN, "--session", self.session])
            os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", self.rows, self.cols, 0, 0))
        self.pump(2.5)
        return self

    def pump(self, secs=0.6):
        deadline = time.time() + secs
        while time.time() < deadline:
            r, _, _ = select.select([self.fd], [], [], 0.1)
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                self.stream.feed(data)
        return self

    def send(self, data, settle=0.7):
        if isinstance(data, str):
            data = data.encode()
        os.write(self.fd, data)
        self.pump(settle)
        return self

    def prefix(self, key, settle=2.0):
        """herdr default prefix is ctrl+b (0x02), then the action key."""
        self.send(b"\x02", settle=0.4)
        self.send(key, settle=settle)
        return self

    def current_pane(self):
        return json.loads(self.cli("pane", "current", "--current").stdout)["result"]["pane"]["pane_id"]

    def split(self, cwd=None, direction="down"):
        args = ["pane", "split", "--current", "--direction", direction, "--focus"]
        if cwd:
            args += ["--cwd", cwd]
        self.cli(*args)
        self.pump(1.0)
        return self.current_pane()

    def report_agent(self, pane, source, agent, sid=None, state="working"):
        args = ["pane", "report-agent", pane, "--source", source, "--agent", agent, "--state", state]
        if sid:
            args += ["--agent-session-id", sid]
        return self.cli(*args)

    def focus_pane(self, pane):
        self.cli("pane", "focus-id", pane) if False else self.cli("agent", "focus", pane)
        self.pump(1.5)

    def grid(self):
        return [row.rstrip() for row in self.screen.display]

    def text(self):
        return "\n".join(self.grid())

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.screen.resize(rows, cols)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.pump(1.2)
        return self

    def stop(self):
        try:
            if self.fd:
                os.write(self.fd, b"\x03")
        except OSError:
            pass
        try:
            self.cli("server", "stop")
        except Exception:
            pass
        try:
            if self.fd:
                os.close(self.fd)
        except OSError:
            pass
        if self._spawner:
            try:
                self._spawner.terminate()
            except Exception:
                pass
        shutil.rmtree(self.root, ignore_errors=True)
