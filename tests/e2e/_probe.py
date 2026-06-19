#!/usr/bin/env python3
"""Throwaway probe: launch herdr in a sandboxed pty, dump the reconstructed grid
so we can see the real startup/onboarding state before writing the E2E harness."""
import os, sys, pty, time, fcntl, struct, termios, tempfile, select, shutil, subprocess

try:
    import pyte
except ImportError:
    sys.exit("need pyte: pip3 install --break-system-packages pyte")

BIN = os.path.expanduser("~/Projects/herdr/target/release/herdr")
ROWS, COLS = 40, 160

home = tempfile.mkdtemp(prefix="herdr-probe-")
cfgdir = os.path.join(home, ".config", "herdr")
os.makedirs(cfgdir, exist_ok=True)
with open(os.path.join(cfgdir, "config.toml"), "w") as f:
    f.write("[ui]\nusage_strip = true\ngoal_panel = true\n")

sock = os.path.join(home, "herdr.sock")
env = dict(
    os.environ,
    HOME=home,
    XDG_CONFIG_HOME=os.path.join(home, ".config"),
    XDG_STATE_HOME=os.path.join(home, ".state"),
    XDG_DATA_HOME=os.path.join(home, ".data"),
    HERDR_SOCKET_PATH=sock,
    TERM="xterm-256color",
)
for k in ("HERDR_CLIENT_SOCKET_PATH",):
    env.pop(k, None)

screen = pyte.Screen(COLS, ROWS)
stream = pyte.ByteStream(screen)
pid, fd = pty.fork()
if pid == 0:
    os.environ.clear(); os.environ.update(env)
    os.execv(BIN, [BIN, "--session", "probe"])
    os._exit(127)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

deadline = time.time() + 4
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        stream.feed(data)

print("===== GRID =====")
for row in screen.display:
    print(repr(row.rstrip()))
print("===== END =====")

# quit + cleanup
for key in (b"q", b"\x03"):
    try: os.write(fd, key); time.sleep(0.3)
    except OSError: pass
try: os.close(fd)
except OSError: pass
subprocess.run([BIN, "server", "stop"], env=env, capture_output=True)
time.sleep(0.3)
shutil.rmtree(home, ignore_errors=True)
