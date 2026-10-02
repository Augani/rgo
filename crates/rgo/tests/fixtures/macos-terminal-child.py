"""Application used by the installed-launcher terminal compatibility fixture."""
import os
import signal
import subprocess
import sys

expected = os.environ.get("RGO_PROBE_STDIO", "111")
actual = "".join("1" if os.isatty(fd) else "0" for fd in range(3))
assert actual == expected, (actual, expected)
with open("/dev/tty", "rb", buffering=0) as tty:
    assert os.tcgetpgrp(tty.fileno()) == os.getpgrp()

def size(prefix):
    rows, cols = os.get_terminal_size(2).lines, os.get_terminal_size(2).columns
    print(f"{prefix}:{rows}:{cols}", flush=True)

signal.signal(signal.SIGWINCH, lambda *_: size("RGO_RESIZED"))
if os.environ.get("RGO_PROBE_BACKGROUND"):
    signal.signal(signal.SIGCONT, lambda *_: os.write(1, b"RGO_CONTINUED\n"))
print(f"\x1b[32mRGO_TTY:{actual}\x1b[0m", flush=True)
print("RGO_TERMINAL_READY", file=sys.stderr, flush=True)
if os.environ.get("RGO_PROBE_LATE"):
    subprocess.Popen([sys.executable, "-c", """
import os, pathlib, time
os.close(0)
pathlib.Path('late-ready').write_text('ready')
deadline = time.monotonic() + 30
while not pathlib.Path('late-release').exists():
    if time.monotonic() >= deadline: raise RuntimeError('late output was not released')
    time.sleep(0.02)
os.write(1, b'RGO_LATE_OUTPUT\\n')
pathlib.Path('late-done').write_text('written')
"""], stdin=subprocess.DEVNULL, start_new_session=True)
    sys.exit(17)
for line in sys.stdin:
    line = line.strip()
    if line == "size":
        size("RGO_SIZE")
    elif line == "quit":
        break
    else:
        print(f"RGO_INPUT:{line}", flush=True)
sys.exit(17)
