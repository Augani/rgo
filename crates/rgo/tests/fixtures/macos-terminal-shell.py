"""Drive an actual interactive zsh in a private PTY; no third-party packages."""
import errno
import fcntl
import json
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
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
shell_sequence = 0
caller_groups = set()
shell_reaped = False

def failure_state():
    # Collect private-fixture state only on failure; keep healthy runs cheap.
    root = pathlib.Path(os.environ["RGO_HOME"])
    details = {"foreground": os.tcgetpgrp(master), "shell": pid,
               "modes": termios.tcgetattr(master), "guardian_stderr": {}}
    try:
        details["events"] = (root / "state/macos-supervisor-audit/events").read_bytes()[-16000:]
    except OSError as error:
        details["events"] = str(error)
    for directory in list((root / "state").glob("macos-cargo-job-*"))[:8]:
        try:
            with (directory / "guardian.stderr").open("rb") as stream:
                details["guardian_stderr"][directory.name] = stream.read(4096)
        except OSError as error:
            details["guardian_stderr"][directory.name] = str(error)
    try:
        rows = subprocess.check_output(
            ["ps", "-axo", "pid=,ppid=,pgid=,stat=,etime=,sigmask=,command="],
            text=True, timeout=2)
        rows = rows.splitlines()
        owned = {str(pid), str(details["foreground"])}
        owned.update(row.split()[0] for row in rows if str(root) in row)
        while True:
            children = {row.split()[0] for row in rows if row.split()[1] in owned}
            if children <= owned:
                break
            owned.update(children)
        details["processes"] = [row[:1024] for row in rows if row.split()[0] in owned][:32]
    except (OSError, subprocess.TimeoutExpired) as error:
        details["processes"] = str(error)
    return details

def expect(value, timeout=20):
    deadline = time.monotonic() + timeout
    while value not in pending:
        if time.monotonic() >= deadline:
            raise RuntimeError(f"missing {value!r}; state={failure_state()!r}; transcript={bytes(transcript)!r}")
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
    if value == b"RGO_TERMINAL_READY":
        group = os.tcgetpgrp(master)
        if group != pid:
            caller_groups.add(group)

def send(value):
    os.write(master, value)

def command(value):
    send(value.encode() + b"\n")

def shell_command(value):
    # Asynchronous job notifications can redraw a second prompt. Synchronize
    # completion with a unique expanded marker, rather than a stale prompt.
    global shell_sequence
    shell_sequence += 1
    command(f"{value}; print -r RGO_SHELL_DONE:$(({shell_sequence}))_")
    expect(f"RGO_SHELL_DONE:{shell_sequence}_".encode())
    expect(prompt)

def status(expected):
    command("print -r RGO_STATUS:$?")
    expect(f"RGO_STATUS:{expected}".encode())
    expect(prompt)

def wait_for(operation, description):
    deadline = time.monotonic() + 10
    while not operation():
        if time.monotonic() >= deadline:
            raise RuntimeError(f"{description}; foreground={os.tcgetpgrp(master)}, shell={pid}, "
                f"modes={termios.tcgetattr(master)}; transcript={bytes(transcript[-6000:])!r}")
        time.sleep(0.02)

def jobs():
    return list((pathlib.Path(os.environ["RGO_HOME"]) / "state").glob("macos-cargo-job-*"))

def retire_private_jobs():
    for directory in jobs():
        try:
            owner = json.loads((directory / "owner.json").read_text())
            target = owner["domain"] + "/" + owner["label"]
            assert owner["domain"] == f"gui/{os.getuid()}"
            assert owner["label"] == "com.rgo.cargo." + owner["token"][:32]
            state = subprocess.run(["launchctl", "print", target], capture_output=True,
                text=True, timeout=3, check=False)
            if state.returncode == 0 and str(directory) in state.stdout:
                guardian = next(int(line.split("=", 1)[1]) for line in state.stdout.splitlines()
                    if line.strip().startswith("pid = "))
                rows = subprocess.check_output(["ps", "-axo", "pid=,ppid=,pgid="],
                    text=True, timeout=3)
                processes = [tuple(map(int, row.split())) for row in rows.splitlines()]
                descendants = {guardian}
                while True:
                    expanded = descendants | {child for child, parent, _ in processes if parent in descendants}
                    if expanded == descendants:
                        break
                    descendants = expanded
                for child, _, group in processes:
                    if child in descendants and child != guardian and group > 0 and group != pid:
                        try:
                            # Recheck live session membership before signaling.
                            if os.getsid(child) == guardian:
                                os.killpg(group, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
            subprocess.run(["launchctl", "bootout", target], stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, timeout=3, check=False)
        except (OSError, subprocess.TimeoutExpired):
            # The guardian can retire between enumeration and this cleanup.
            pass

try:
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    expect(prompt)
    shell_command("precmd() { : 'existing prompt function'; }; rgo_existing_preexec() { print -r -- \"$1\" > existing-hook-command; }; preexec_functions=(rgo_existing_preexec)")
    shell_command('eval "$("$RGO_TERMINAL_HELPER" macos-terminal-host init-zsh)"')
    shell_command('eval "$("$RGO_TERMINAL_HELPER" macos-terminal-host init-zsh)"')
    shell_command('[[ ${functions[precmd]} == *"existing prompt function"* && ${preexec_functions[(Ie)rgo_existing_preexec]} -gt 0 && -n $RGO_TERMINAL_HOST ]] || exit 91')
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
        shell_command("bg")
        deadline = time.monotonic() + 10
        while True:
            shell_command("jobs -s > background-state")
            if b"suspended" in pathlib.Path("background-state").read_bytes():
                break
            if time.monotonic() >= deadline:
                raise RuntimeError(f"background job did not stop; state={failure_state()!r}; "
                    f"jobs={pathlib.Path('background-state').read_bytes()!r}; transcript={bytes(transcript[-6000:])!r}")
            time.sleep(0.02)
        command("fg")
        expect(b"RGO_CONTINUED")
        send(b"quit\n")
        expect(prompt)
        status(17)
    command("stty -tostop")
    expect(prompt)

    # `fg` of an already-running zsh job sends no SIGCONT. A silent job
    # must still acquire relay mode before any input, output, or exit wakes it.
    command("RGO_PROBE_IDLE_BACKGROUND=1 cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    wait_for(lambda: pathlib.Path("idle-background-ready").exists(), "idle process did not start")
    send(b"\x1a")
    expect(prompt)
    shell_command("bg")
    shell_command("jobs -r > background-state")
    assert b"running" in pathlib.Path("background-state").read_bytes(), "idle background job was not running"
    command("fg")
    def foreground_ready():
        # An actual terminal keeps consuming shell output during a handoff.
        # Preserve it for the subsequent expectations and failure diagnostics.
        if select.select([master], [], [], 0)[0]:
            chunk = os.read(master, 8192)
            pending.extend(chunk)
            transcript.extend(chunk)
        return os.tcgetpgrp(master) != pid and termios.tcgetattr(master)[3] & (termios.ICANON | termios.ECHO) == 0
    wait_for(foreground_ready,
        "silent running background job did not enter foreground relay mode")
    pathlib.Path("idle-background-release").write_text("continue")
    send(b"quit\n")
    expect(prompt)
    status(17)

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
    hosts = list((pathlib.Path(os.environ["RGO_HOME"]) / "state/terminal-hosts").iterdir())
    assert len(hosts) == 1
    host = hosts[0]
    crash_lease = json.loads((host / "lease.json").read_text())
    os.kill(caller_group, signal.SIGKILL)
    expect(prompt)
    status(137)
    wait_for(lambda: not jobs(), "guardian did not recover after caller SIGKILL")
    command("stty -g > crash-terminal-mode")
    expect(prompt)
    before = pathlib.Path("before-crash-terminal-mode").read_bytes()
    after = pathlib.Path("crash-terminal-mode").read_bytes()
    assert before == after, f"registered shell did not recover after caller SIGKILL: before={before!r}, after={after!r}"

    # Replay the actual saved lease after the original caller has gone. Even a
    # matching raw-mode shape cannot authorize a later command or a reused PID.
    pathlib.Path("saved-terminal-lease.json").write_text(json.dumps(crash_lease))
    pathlib.Path("stale-terminal-lease.py").write_text('''import json, pathlib, sys, tty
host = pathlib.Path(sys.argv[1])
lease = json.loads(pathlib.Path('saved-terminal-lease.json').read_text())
if sys.argv[2] == 'reused':
    lease['generation'] = int((host / 'generation').read_text())
    lease['caller'] = json.loads((host / 'host.json').read_text())['shell']
    lease['caller']['started_microseconds'] = (lease['caller']['started_microseconds'] + 1) % 1000000
path = host / 'lease.json'
path.write_text(json.dumps(lease))
path.chmod(0o600)
with open('/dev/tty', 'rb', buffering=0) as terminal:
    tty.setraw(terminal.fileno())
''')
    for reason in ("generation", "reused"):
        shell_command(f"python3 stale-terminal-lease.py '{host}' {reason}")
        assert not (host / "lease.json").exists(), f"{reason} lease was not retired"
        shell_command("stty -g > refused-terminal-mode")
        assert pathlib.Path("refused-terminal-mode").read_bytes() != before, f"{reason} lease overwrote a later raw mode"
        shell_command('stty "$(cat before-crash-terminal-mode)"')
    shell_command("stty -g > recovered-terminal-mode")
    assert pathlib.Path("recovered-terminal-mode").read_bytes() == before
    shell_command("stty -ixon; stty -g > intentional-terminal-edit")
    shell_command("stty -g > subsequent-terminal-mode")
    assert pathlib.Path("intentional-terminal-edit").read_bytes() == pathlib.Path("subsequent-terminal-mode").read_bytes(), "recovery overwrote a later terminal edit"
    assert b"guardian unavailable" not in transcript, bytes(transcript)
    shell_command('precmd_functions=(${precmd_functions:#__rgo_terminal_precmd})')
    command("cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    send(b"quit\n")
    expect(prompt)
    status(17)
    shell_command('[[ $RGO_TERMINAL_HOST == disabled ]] || exit 93')
    assert b"guardian unavailable" in transcript, "removed finalizer did not refuse admission"
    assert not jobs(), "removed finalizer admitted an owned guardian"
    shell_command('__rgo_terminal_undo; [[ -z $RGO_TERMINAL_HOST && ${functions[precmd]} == *"existing prompt function"* && ${preexec_functions[(Ie)rgo_existing_preexec]} -gt 0 ]] || exit 92')
    assert not host.exists(), "undo retained the owned terminal host"
    shell_command('eval "$("$RGO_TERMINAL_HELPER" macos-terminal-host init-zsh)"')
    assert int(next((pathlib.Path(os.environ["RGO_HOME"]) / "state/terminal-hosts").iterdir()).joinpath("generation").read_text()) == 0
    # Leave a real process in this shell's session after its leader exits. It
    # ignores HUP and closes every standard descriptor; Rust will release it.
    pathlib.Path("shell-worker.py").write_text('''import os, pathlib, signal, time
signal.signal(signal.SIGHUP, signal.SIG_IGN)
for fd in (0, 1, 2): os.close(fd)
pathlib.Path('shell-worker-pid').write_text(str(os.getpid()))
deadline = time.monotonic() + 120
while not pathlib.Path('release-shell-worker').exists() and time.monotonic() < deadline:
    time.sleep(0.02)
pathlib.Path('shell-worker-exited').write_text('finished')
''')
    shell_command("python3 shell-worker.py &")
    wait_for(lambda: pathlib.Path("shell-worker-pid").is_file(), "shell worker did not start")
    command("cargo run --offline")
    expect(b"RGO_TERMINAL_READY")
    host = next((pathlib.Path(os.environ["RGO_HOME"]) / "state/terminal-hosts").iterdir())
    # Added user content prevents background retirement until Rust has checked
    # ownership and a live caller, then interrupted an actual journaled removal.
    (host / "keep-me").write_text("user content")
    pathlib.Path("retiring-terminal-token").write_text(host.name)
    pathlib.Path("retiring-terminal-lease.json").write_bytes((host / "lease.json").read_bytes())
    # Disconnect an active physical terminal, then await actual shell exit and
    # guardian retirement. There is no terminal left whose modes may be reset.
    os.close(master)
    master = None
    wait_for(lambda: not jobs(), "terminal disconnect retained an active guardian")
    deadline = time.monotonic() + 10
    while True:
        exited, exit_status = os.waitpid(pid, os.WNOHANG)
        if exited == pid:
            shell_reaped = True
            # zsh 5.9 handles SIGHUP via zexit(SIGHUP, ZEXIT_SIGNAL):
            # its normal exit code is the signal number, not 128 + SIGHUP.
            assert os.waitstatus_to_exitcode(exit_status) == signal.SIGHUP, exit_status
            break
        assert time.monotonic() < deadline, "disconnected shell did not exit"
        time.sleep(0.02)
    print("installed Cargo pilot: controlling /dev/tty, isatty/color, input/EOF, resize, Ctrl-Z/fg/bg, TOSTOP, Ctrl-C, exit status, normal terminal restoration, mixed pipes/redirection, late output, caller-crash job cleanup, registered zsh recovery, stale generations/PID identities, preserved hooks, undo, and active-terminal disconnect passed")
finally:
    # A failed assertion must also retire this private shell's stopped jobs.
    # Their process groups differ from the shell group used by pty.fork().
    for group in caller_groups:
        try:
            if os.getsid(group) == pid:
                os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        retire_private_jobs()
    finally:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if master is not None:
            os.close(master)
        if not shell_reaped:
            os.waitpid(pid, 0)
