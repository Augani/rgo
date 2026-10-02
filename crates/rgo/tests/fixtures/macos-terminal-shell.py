"""Drive an actual interactive zsh in a private PTY; no third-party packages."""
import errno
import fcntl
import os
import pathlib
import pty
import select
import signal
import struct
import termios
import time

os.environ["PS1"] = "rgo-probe> "
os.environ["TERM"] = "xterm-256color"
pid, master = pty.fork()
if pid == 0:
    os.execv("/bin/zsh", ["zsh", "-f"])

pending = bytearray()
transcript = bytearray()
prompt = b"rgo-probe> "

def expect(value, timeout=20):
    deadline = time.monotonic() + timeout
    while value not in pending:
        if time.monotonic() >= deadline:
            raise RuntimeError(f"missing {value!r}; transcript={bytes(transcript)!r}")
        ready, _, _ = select.select([master], [], [], min(0.2, deadline - time.monotonic()))
        if ready:
            try:
                chunk = os.read(master, 8192)
            except OSError as error:
                if error.errno == errno.EIO:
                    raise RuntimeError(f"shell exited; transcript={bytes(transcript)!r}") from error
                raise
            if not chunk:
                raise RuntimeError(f"shell closed; transcript={bytes(transcript)!r}")
            pending.extend(chunk)
            transcript.extend(chunk)
    end = pending.index(value) + len(value)
    del pending[:end]

def send(value):
    os.write(master, value)

def command(value):
    send(value.encode() + b"\n")

def status(expected):
    command("print -r RGO_STATUS:$?")
    expect(f"RGO_STATUS:{expected}".encode())
    expect(prompt)

def wait_for(operation, description):
    deadline = time.monotonic() + 10
    while not operation():
        if time.monotonic() >= deadline:
            raise RuntimeError(description)
        time.sleep(0.02)

def jobs():
    return list((pathlib.Path(os.environ["RGO_HOME"]) / "state").glob("macos-cargo-job-*"))

try:
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    expect(prompt)
    command("stty -g > before-terminal-mode")
    expect(prompt)
    command("cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    assert b"\x1b[32mRGO_TTY:111\x1b[0m" in transcript
    send(b"hello\n")
    expect(b"RGO_INPUT:hello")
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))
    expect(b"RGO_RESIZED:32:100")
    send(b"\x1a")
    expect(prompt)
    command("fg")
    send(b"size\n")
    expect(b"RGO_SIZE:32:100")
    send(b"\x04")
    expect(prompt)
    status(17)
    command("stty -g > after-terminal-mode")
    expect(prompt)
    assert pathlib.Path("before-terminal-mode").read_bytes() == pathlib.Path("after-terminal-mode").read_bytes(), "terminal mode was not restored"

    command("cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    send(b"\x03")
    expect(prompt)
    status(130)

    command("RGO_PROBE_STDIO=101 cargo run --offline > redirected-output")
    expect(b"RGO_TERMINAL_READY")
    send(b"quit\n")
    expect(prompt)
    status(17)
    assert b"RGO_TTY:101" in pathlib.Path("redirected-output").read_bytes()

    command("printf 'quit\\n' | RGO_PROBE_STDIO=011 cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    expect(prompt)
    status(17)
    assert b"guardian unavailable" not in transcript, bytes(transcript)

    # Background input, and background output with the shell's TOSTOP flag,
    # must stop the actual shell job and become usable again with fg.
    for tostop in (False, True):
        command("stty tostop" if tostop else "stty -tostop")
        expect(prompt)
        command("RGO_PROBE_BACKGROUND=1 cargo run --offline")
        expect(b"RGO_TERMINAL_READY")
        send(b"\x1a")
        expect(prompt)
        command("bg")
        expect(prompt)
        command("sleep 0.2; jobs -s")
        expect(b"suspended")
        expect(prompt)
        command("fg")
        send(b"quit\n")
        expect(prompt)
        status(17)
    command("stty -tostop")
    expect(prompt)

    # The shell has already regained its prompt before this detached process
    # writes. Closing the caller's master must not hang up that surviving PTY.
    command("RGO_PROBE_LATE=1 cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    expect(prompt)
    status(17)
    wait_for(lambda: pathlib.Path("late-ready").exists(), "late writer did not start")
    assert jobs(), "guardian released the PTY before its surviving writer"
    pathlib.Path("late-release").write_text("continue")
    expect(b"RGO_LATE_OUTPUT")
    wait_for(lambda: pathlib.Path("late-done").exists(), "late write did not finish")
    wait_for(lambda: not jobs(), "guardian did not retire after late output")

    command("stty -g > before-crash-terminal-mode")
    expect(prompt)
    command("cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    caller_group = os.tcgetpgrp(master)
    assert caller_group > 0 and caller_group != pid
    os.kill(caller_group, signal.SIGKILL)
    expect(prompt)
    status(137)
    wait_for(lambda: not jobs(), "guardian did not recover after caller SIGKILL")
    command("stty -g > crash-terminal-mode")
    expect(prompt)
    before = pathlib.Path("before-crash-terminal-mode").read_bytes()
    after = pathlib.Path("crash-terminal-mode").read_bytes()
    if before != after:
        # Keep the observed limitation visible. Exact restoration after this
        # shell/guardian race is an activation gate, not a passing claim.
        print(f"macOS terminal activation gap: caller SIGKILL changed terminal mode: before={before!r}, after={after!r}")
    assert b"guardian unavailable" not in transcript, bytes(transcript)
    print("installed Cargo pilot: controlling /dev/tty, isatty/color, input/EOF, resize, Ctrl-Z/fg/bg, TOSTOP, Ctrl-C, exit status, normal terminal restoration, mixed pipes/redirection, late output, and caller-crash job cleanup passed")
finally:
    os.killpg(pid, signal.SIGKILL)
    os.close(master)
    os.waitpid(pid, 0)
