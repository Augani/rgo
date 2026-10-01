#!/usr/bin/env python3
"""Probe nightly Cargo build layouts in private rgo homes.

This checks relocation, final outputs, rgo's incremental tier, and whether
the documented profile-lock heuristic is available. It is not evidence that
whole-context GC is safe.
The held-lock check requires Unix; the exact-version no-lock capability check
also runs on Windows. It never touches the developer's Cargo configuration or
installs a service.
"""

import argparse
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

if os.name != "nt":
    import fcntl


ROOT = Path(__file__).resolve().parent.parent
RGO = ROOT / "target" / "debug" / ("rgo.exe" if os.name == "nt" else "rgo")


def run(args: list[str], *, cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"{' '.join(args)} failed:\n{result.stdout}\n{result.stderr}")
    return result


def probe(mode: str, rustup_home: str, toolchain: str, expect_profile_lock: bool) -> None:
    with tempfile.TemporaryDirectory(prefix=f"rgo-{mode}-") as raw:
        root = Path(raw)
        home = root / "home"
        project = root / "project"
        cargo_home = home / ".cargo"
        rgo_home = home / ".rgo"
        cargo_home.mkdir(parents=True)
        (project / "src").mkdir(parents=True)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "rgo_layout_probe"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (project / "src" / "main.rs").write_text("fn main() {}\n")
        (project / "build.rs").write_text("fn main() {}\n")

        env = os.environ.copy()
        env.update(
            HOME=str(home),
            USERPROFILE=str(home),
            CARGO_HOME=str(cargo_home),
            RGO_HOME=str(rgo_home),
            RUSTUP_HOME=rustup_home,
            RUSTUP_TOOLCHAIN=toolchain,
            CARGO_INCREMENTAL="1",
        )
        for variable in (
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_BUILD_DIR",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
        ):
            env.pop(variable, None)

        run([str(RGO), "setup", "--no-service"], cwd=project, env=env)
        command = ["cargo"]
        if mode == "new-layout":
            command.extend(["-Z", "build-dir-new-layout"])
        command.extend(["build", "--offline", "--message-format=json"])
        build = run(command, cwd=project, env=env)
        build_root = (rgo_home / "builds").resolve()
        out_dirs = [
            Path(message["out_dir"]).resolve()
            for line in build.stdout.splitlines()
            if line.startswith("{")
            for message in [json.loads(line)]
            if message.get("reason") == "build-script-executed"
        ]
        if not out_dirs or not all(path.is_relative_to(build_root) for path in out_dirs):
            raise RuntimeError(f"{mode}: build script outputs did not use managed storage")
        executable = project / "target" / "debug" / (
            "rgo_layout_probe.exe" if os.name == "nt" else "rgo_layout_probe"
        )
        if not executable.is_file():
            raise RuntimeError(f"{mode}: final executable did not stay in target/debug")
        listing = run([str(RGO), "ls"], cwd=project, env=env).stdout
        if str(project.resolve()) not in listing:
            raise RuntimeError(f"{mode}: rgo did not discover the Cargo build context")
        sidecars = list(build_root.rglob(".rgo-context.json"))
        if len(sidecars) != 1:
            raise RuntimeError(f"{mode}: expected one rgo-owned context sidecar, found {sidecars}")
        context = sidecars[0].parent.resolve()
        if not all(path.is_relative_to(context) for path in out_dirs):
            raise RuntimeError(f"{mode}: Cargo output escaped the attributed context")
        profile = context / "debug"
        incremental = profile / "incremental"
        lock = profile / ".cargo-build-lock"
        if not incremental.is_dir() or not any(incremental.iterdir()):
            raise RuntimeError(f"{mode}: documented debug incremental tier is missing or empty")
        if expect_profile_lock and not lock.is_file():
            raise RuntimeError(
                f"{mode}: documented debug Cargo build lock is missing; "
                f"profile entries: {[entry.name for entry in profile.iterdir()]}"
            )
        if not expect_profile_lock and lock.exists():
            raise RuntimeError(f"{mode}: profile lock appeared; update the pinned capability fixture")
        if mode == "default":
            doctor = json.loads(
                run([str(RGO), "doctor", "--verify", "--json"], cwd=project, env=env).stdout
            )
            lock_entry = next(
                (
                    entry
                    for entry in doctor["entries"]
                    if "documented profile build locks in the disposable project" in entry["message"]
                    or "documented debug/release profile build locks in the disposable project"
                    in entry["message"]
                ),
                None,
            )
            expected_level = "ok" if expect_profile_lock else "warning"
            if (
                doctor["schema_version"] != 2
                or doctor["activation_verified"] is not True
                or doctor["profile_locks_observed"] is not expect_profile_lock
                or lock_entry is None
                or lock_entry["level"] != expected_level
            ):
                raise RuntimeError(f"{mode}: doctor misreported Cargo's lock capability: {doctor}")
        status = run([str(RGO), "status"], cwd=project, env=env).stdout
        incremental_line = next(
            (line for line in status.splitlines() if line.strip().startswith("incremental state")),
            "",
        )
        if not incremental_line or float(incremental_line.split()[-2]) <= 0:
            raise RuntimeError(f"{mode}: rgo did not count incremental state:\n{status}")

        if expect_profile_lock:
            # Age the completed build's lock timestamp to isolate the held
            # lock check from the separate conservative recency heuristic.
            old = time.time() - 7200
            os.utime(lock, (old, old))
            gc_args = [str(RGO), "gc", "--dry-run", "--aggressive", "--target", "0"]
            with lock.open("r+b") as lock_file:
                fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                locked_plan = run(gc_args, cwd=project, env=env).stdout
                fcntl.flock(lock_file, fcntl.LOCK_UN)
            if "1 context(s) skipped: recently changed or held Cargo profile lock" not in locked_plan:
                raise RuntimeError(f"{mode}: rgo did not protect the held profile lock:\n{locked_plan}")
            idle_plan = run(gc_args, cwd=project, env=env).stdout
            if (
                "1 context(s) skipped: workspace attribution, availability, or supervised origin unverified"
                not in idle_plan
                or "nothing to reclaim" not in idle_plan
                or not context.is_dir()
            ):
                raise RuntimeError(f"{mode}: native-origin context was not protected:\n{idle_plan}")
        else:
            # Older Cargo still relocates its intermediates, but rgo must not
            # infer a safe native deletion protocol from an unfamiliar lock.
            deletion = subprocess.run(
                [str(RGO), "gc", "--aggressive", "--target", "0"],
                cwd=project,
                env=env,
                text=True,
                capture_output=True,
            )
            if deletion.returncode == 0 or not context.is_dir():
                raise RuntimeError(f"{mode}: native deletion was not refused:\n{deletion.stdout}\n{deletion.stderr}")
        run([str(RGO), "setup", "--undo", "--no-service"], cwd=project, env=env)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--toolchain", default="nightly")
    parser.add_argument("--without-profile-lock", action="store_true")
    args = parser.parse_args()
    if os.name == "nt" and not args.without_profile_lock:
        raise RuntimeError("held profile-lock verification requires Unix")
    if not RGO.is_file():
        raise RuntimeError("build the workspace before running the nightly layout probe")
    rustup_home = subprocess.run(
        ["rustup", "show", "home"], check=True, capture_output=True, text=True
    ).stdout.strip()
    version = subprocess.run(
        ["cargo", f"+{args.toolchain}", "--version"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    for mode in ("default", "new-layout"):
        probe(mode, rustup_home, args.toolchain, not args.without_profile_lock)
    print(
        f"{version}: relocation, final outputs, incremental state, and "
        f"{'held-lock detection and native-origin protection' if not args.without_profile_lock else 'native deletion refusal without a supported profile lock'} "
        "verified in both layout modes"
    )


if __name__ == "__main__":
    main()
