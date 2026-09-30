#!/usr/bin/env python3
"""Exercise bounded automatic cleanup after 100 unchanged Cargo builds.

The probe uses private HOME/CARGO_HOME/RGO_HOME, a supervised Cargo shim, and
real offline builds. Its debug daemon marker counts completed maintenance
cycles; wall-clock time is recorded but is not the pass criterion.
"""

import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
RGO = ROOT / "target" / "debug" / ("rgo.exe" if os.name == "nt" else "rgo")
PROJECTS = 100
BUDGET = "4MB"


def run(args: list[str], env: dict[str, str], *, cwd: Path) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(
            f"{' '.join(args)} exited {result.returncode}:\n"
            f"{result.stdout[-2000:]}\n{result.stderr[-2000:]}"
        )
    return result


def contexts(rgo_home: Path) -> list[Path]:
    builds = rgo_home / "builds"
    return [
        context
        for shard in builds.iterdir()
        if shard.is_dir()
        for context in shard.iterdir()
        if context.is_dir()
    ]


def start_daemon(root: Path, env: dict[str, str]):
    log = (root / "daemon.log").open("ab")
    process = subprocess.Popen(
        [str(RGO), "daemon", "--foreground"],
        cwd=root,
        env=env,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    return process, log


def stop_daemon(process, log) -> None:
    process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
    log.close()


def wait_ready(process, env: dict[str, str], root: Path) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"daemon exited {process.returncode} before readiness")
        report = subprocess.run(
            [str(RGO), "doctor", "--json"],
            cwd=root,
            env=env,
            text=True,
            capture_output=True,
        )
        if report.returncode == 0:
            try:
                entries = json.loads(report.stdout)["entries"]
                if any(
                    entry["level"] == "ok"
                    and entry["message"].startswith("daemon responds with protocol")
                    for entry in entries
                ):
                    return
            except (KeyError, ValueError, TypeError):
                pass
        time.sleep(0.1)
    raise RuntimeError("daemon did not become ready within 30 seconds")


def age_profile_locks(build_contexts: list[Path]) -> int:
    old = time.time() - 7200
    aged = 0
    for context in build_contexts:
        for profile in context.iterdir():
            lock = profile / ".cargo-build-lock"
            if profile.is_dir() and lock.is_file():
                os.utime(lock, (old, old))
                aged += 1
    return aged


def tick_count(path: Path) -> int:
    return path.read_bytes().count(b"tick\n") if path.is_file() else 0


def probe(root: Path) -> None:
    home = root / "home"
    cargo_home = home / ".cargo"
    rgo_home = home / ".rgo"
    cargo_home.mkdir(parents=True)
    real_cargo = shutil.which("cargo")
    if real_cargo is None or not RGO.is_file():
        raise RuntimeError("build the workspace and put rustup's cargo proxy on PATH first")
    rustup_home = subprocess.run(
        ["rustup", "show", "home"], check=True, text=True, capture_output=True
    ).stdout.strip()
    env = os.environ.copy()
    env.update(
        HOME=str(home),
        USERPROFILE=str(home),
        CARGO_HOME=str(cargo_home),
        RGO_HOME=str(rgo_home),
        RUSTUP_HOME=rustup_home,
        RUSTUP_TOOLCHAIN="stable",
    )
    for variable in (
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_BUILD_DIR",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RGO_TEST_MAINTENANCE_TICK_LOG",
    ):
        env.pop(variable, None)
    run(
        [str(RGO), "setup", "--supervised", "--real-cargo", str(Path(real_cargo).absolute()), "--no-service"],
        env,
        cwd=root,
    )
    record = json.loads((cargo_home / ".rgo-install.json").read_text())
    shim = Path(record["supervised_cargo"]["shim_path"])
    env["PATH"] = str(shim.parent) + os.pathsep + env["PATH"]
    # On Windows, CreateProcess resolves an unqualified executable against the
    # parent's PATH, not the env= override passed to subprocess.run.
    os.environ["PATH"] = env["PATH"]
    selected_cargo = shutil.which("cargo")
    if selected_cargo is None or not os.path.samefile(selected_cargo, shim):
        raise RuntimeError(f"PATH did not select the supervised Cargo launcher: {selected_cargo}")
    config = rgo_home / "config.toml"
    config.write_text("[storage]\nmax_size = '1GB'\nmin_free_space = '0B'\n[gc]\nauto = true\n")

    first = None
    second = None
    try:
        env["RGO_DAEMON_POLL_SECS"] = "3600"
        first = start_daemon(root, env)
        wait_ready(first[0], env, root)
        start = time.monotonic()
        for index in range(PROJECTS):
            project = root / f"project-{index:03}"
            (project / "src").mkdir(parents=True)
            (project / "Cargo.toml").write_text(
                f'[package]\nname = "bulk-{index:03}"\nversion = "0.1.0"\nedition = "2021"\n'
            )
            (project / "src" / "main.rs").write_text("fn main() {}\n")
            run(["cargo", "build", "--offline", "--quiet"], env, cwd=project)
            if index == 0 and len(contexts(rgo_home)) != 1:
                raise RuntimeError("first build bypassed supervised Cargo storage")
        before = contexts(rgo_home)
        if len(before) != PROJECTS:
            raise RuntimeError(f"expected {PROJECTS} managed contexts, got {len(before)}")
        pending = list((rgo_home / "state" / "pending-maintenance").iterdir())
        if len(pending) != PROJECTS:
            raise RuntimeError(f"expected {PROJECTS} pending launches, got {len(pending)}")
        print(f"built {PROJECTS} projects in {time.monotonic() - start:.1f}s", flush=True)
        stop_daemon(*first)
        first = None

        aged = age_profile_locks(before)
        if aged < PROJECTS:
            raise RuntimeError(f"only {aged} completed Cargo profile locks were found")
        config.write_text(f"[storage]\nmax_size = '{BUDGET}'\nmin_free_space = '0B'\n[gc]\nauto = true\n")
        marker = root / "maintenance-ticks.log"
        env["RGO_DAEMON_POLL_SECS"] = "1"
        env["RGO_TEST_MAINTENANCE_TICK_LOG"] = str(marker)
        second = start_daemon(root, env)
        wait_ready(second[0], env, root)
        deadline = time.monotonic() + 60
        while tick_count(marker) < 2 and time.monotonic() < deadline:
            if second[0].poll() is not None:
                raise RuntimeError(f"maintenance daemon exited {second[0].returncode}")
            time.sleep(0.05)
        if tick_count(marker) < 2:
            raise RuntimeError("two maintenance cycles did not complete within 60 seconds")
        remaining = contexts(rgo_home)
        status = run([str(RGO), "status"], env, cwd=root).stdout
        if "Over budget" in status:
            raise RuntimeError(f"budget still unmet after two cycles:\n{status}")
        for index in range(PROJECTS):
            executable = root / f"project-{index:03}" / "target" / "debug" / (
                f"bulk-{index:03}.exe" if os.name == "nt" else f"bulk-{index:03}"
            )
            if not executable.is_file():
                raise RuntimeError(f"checkout output disappeared: {executable}")
        print(
            f"two completed cycles: {len(remaining)} managed contexts remain; "
            f"budget met and {PROJECTS} checkout executables preserved",
            flush=True,
        )
    finally:
        if second is not None:
            stop_daemon(*second)
        if first is not None:
            stop_daemon(*first)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="rgo-100-project-") as raw:
        root = Path(raw)
        try:
            probe(root)
        except Exception:
            log = root / "daemon.log"
            if log.is_file():
                print(f"daemon log tail:\n{log.read_text(errors='replace')[-4000:]}", flush=True)
            raise


if __name__ == "__main__":
    main()
