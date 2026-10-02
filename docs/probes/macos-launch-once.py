"""Observe launchd's one-use job retirement; never activates a Cargo home."""

import ctypes
import json
import os
import pathlib
import platform
import plistlib
import signal
import subprocess
import sys
import tempfile
import time
import uuid


def wait_for(predicate, description, seconds=8):
    deadline = time.monotonic() + seconds
    while not predicate():
        if time.monotonic() >= deadline:
            raise RuntimeError(description)
        time.sleep(0.02)


def worker(root):
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    for fd in (0, 1, 2):
        os.close(fd)
    (root / "worker-pid").write_text(str(os.getpid()))
    wait_for(lambda: (root / "release-worker").exists(), "writer not released", 60)
    (root / "late-write").write_text("detached writer survived")


def job(root):
    child = subprocess.Popen([sys.executable, __file__, "worker", str(root)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, start_new_session=True)
    wait_for(lambda: (root / "worker-pid").exists(), "worker did not start")
    (root / "job-pid").write_text(str(os.getpid()))
    wait_for(lambda: (root / "exit-job").exists(), "job not released", 60)
    # Deliberately leave the detached, closed-FD child alive.
    assert child.poll() is None


def observer():
    proc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    proc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                 ctypes.c_void_p, ctypes.c_int]
    proc.proc_pidinfo.restype = ctypes.c_int
    system = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    query = system.coalition_info_resource_usage
    query.argtypes = [ctypes.c_uint64, ctypes.c_void_p, ctypes.c_size_t]
    query.restype = ctypes.c_int

    def membership(pid):
        info = (ctypes.c_uint64 * 5)()
        count = proc.proc_pidinfo(pid, 20, 0, info, ctypes.sizeof(info))
        if count <= 0:
            return None
        assert count == ctypes.sizeof(info) and info[0] > 0
        return info[0]

    def usage(coalition):
        counters = (ctypes.c_uint64 * 2)()
        ctypes.set_errno(0)
        result = query(coalition, counters, ctypes.sizeof(counters))
        return {"result": result, "errno": ctypes.get_errno(),
                "started": counters[0], "exited": counters[1],
                "active": counters[0] - counters[1] if result == 0 else None}

    return membership, usage


def launchctl(*args):
    return subprocess.run(["/bin/launchctl", *args], capture_output=True,
                          timeout=3, check=False)


def audit(mode):
    membership, usage = observer()
    with tempfile.TemporaryDirectory(prefix="rgo-launch-once-") as temporary:
        root = pathlib.Path(temporary)
        label = "com.rgo.probe.once." + uuid.uuid4().hex
        target = f"gui/{os.getuid()}/{label}"
        plist = root / "job.plist"
        definition = plistlib.dumps({"Label": label, "ProgramArguments":
            [sys.executable, str(pathlib.Path(__file__).resolve()), "job", str(root)],
            "RunAtLoad": True, "LaunchOnlyOnce": True,
            "AbandonProcessGroup": True, "ProcessType": "Interactive",
            "StandardOutPath": str(root / "stdout"),
            "StandardErrorPath": str(root / "stderr")})
        plist.write_bytes(definition)
        plist.chmod(0o600)
        parent = None
        child = None
        try:
            loaded = launchctl("bootstrap", f"gui/{os.getuid()}", str(plist))
            assert loaded.returncode == 0, loaded.stderr.decode(errors="replace")
            wait_for(lambda: (root / "job-pid").exists(), "job did not start")
            parent = int((root / "job-pid").read_text())
            child = int((root / "worker-pid").read_text())
            coalition = membership(parent)
            assert coalition == membership(child) and coalition is not None
            before = usage(coalition)
            assert before["active"] == 2
            if mode == "exit":
                (root / "exit-job").write_text("exit")
            else:
                os.kill(parent, signal.SIGKILL)
            wait_for(lambda: membership(parent) is None, "job did not exit")
            # print's exit status is an observation, never a production parser.
            wait_for(lambda: launchctl("print", target).returncode != 0,
                     "launch-once registration did not disappear")
            live = usage(coalition)
            assert live["active"] == 1 and membership(child) == coalition
            (root / "release-worker").write_text("release")
            wait_for(lambda: (root / "late-write").exists(), "detached late write failed")
            wait_for(lambda: membership(child) is None, "writer did not exit")
            # Give launchd its asynchronous termination/reaping notification.
            wait_for(lambda: usage(coalition)["result"] != 0,
                     "empty coalition was not reaped")
            return {"mode": mode, "coalition": coalition, "before": before,
                    "after_job_exit_with_writer_alive": live,
                    "after_writer_exit": usage(coalition),
                    "late_write": (root / "late-write").read_text()}
        finally:
            (root / "release-worker").write_text("release")
            (root / "exit-job").write_text("exit")
            if parent is not None:
                wait_for(lambda: membership(parent) is None, "job cleanup did not finish")
            if child is not None:
                wait_for(lambda: membership(child) is None, "writer cleanup did not finish")
            # Only the cryptographically unique registration made by this probe.
            assert plist.read_bytes() == definition
            removed = launchctl("bootout", target)
            assert removed.returncode in (0, 3), removed.stderr.decode(errors="replace")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] in ("job", "worker"):
        globals()[sys.argv[1]](pathlib.Path(sys.argv[2]))
    else:
        assert platform.system() == "Darwin"
        data = {"platform": platform.platform(), "cases": [audit("exit"), audit("sigkill")]}
        text = json.dumps(data, indent=2) + "\n"
        if len(sys.argv) == 2:
            pathlib.Path(sys.argv[1]).write_text(text)
        print(text, end="")
