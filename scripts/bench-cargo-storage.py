#!/usr/bin/env python3
"""Measure Cargo build latency with and without supervised rgo storage.

Both modes use the same disposable crate and private Cargo home. The compiler
cache and automatic GC remain disabled. This is a latency probe, not a claim
about storage savings or production workload performance.
"""

import argparse
import hashlib
import json
import math
import os
import pathlib
import platform
import re
import shutil
import statistics
import subprocess
import tempfile
import time


REPO = pathlib.Path(__file__).resolve().parent.parent


def command(argv, cwd, env, capture=None):
    started = time.perf_counter_ns()
    result = subprocess.run(argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if result.returncode:
        raise RuntimeError(
            f"{' '.join(map(str, argv))} failed ({result.returncode}):\n"
            + result.stderr.decode(errors="replace")[-4000:]
        )
    if env.get("RGO_MACOS_SUPERVISOR_PILOT") == "1" and b"guardian unavailable" in result.stderr:
        raise RuntimeError("latency sample fell back outside the macOS guardian")
    if capture is not None:
        lines = re.sub(r"\x1b\[[0-9;]*m", "", result.stderr.decode(errors="replace")).splitlines()
        capture.append({
            "elapsed_ms": round(elapsed_ms, 1),
            "stages": [line for line in lines if "macOS Cargo guardian " in line
                       or "supervised Cargo " in line],
        })
    return elapsed_ms, result.stdout.decode(errors="replace").strip()


def summary(values):
    ordered = sorted(values)
    return {
        "median_ms": round(statistics.median(values), 1),
        "p95_ms": round(ordered[math.ceil(0.95 * len(ordered)) - 1], 1),
        "samples_ms": [round(value, 1) for value in values],
    }


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def paired_samples(plain, managed, project, env, count, edit, capture=None):
    samples = {"plain": [], "supervised": []}
    for index in range(count):
        if edit:
            (project / "src/main.rs").write_text(
                f'fn main() {{ println!("edit {index}"); }}\n'
            )
        order = [("plain", plain), ("supervised", managed)]
        if index % 2:
            order.reverse()
        for mode, argv in order:
            elapsed, _ = command(argv, project, env, capture if mode == "supervised" else None)
            samples[mode].append(elapsed)
    return {mode: summary(values) for mode, values in samples.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rgo-bin", type=pathlib.Path, default=REPO / "target/debug/rgo")
    parser.add_argument("--real-cargo", type=pathlib.Path, default=shutil.which("cargo"))
    parser.add_argument("--warm-samples", type=int, default=24)
    parser.add_argument("--edit-samples", type=int, default=12)
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument("--macos-guardian", action="store_true", help="Measure the private macOS launchd guardian instead of descriptor-only supervision")
    parser.add_argument("--profile-guardian", action="store_true", help="Record debug timing stages separately; requires --macos-guardian")
    args = parser.parse_args()
    if not args.real_cargo or not args.real_cargo.is_absolute():
        parser.error("--real-cargo must name an absolute Cargo executable")
    if args.warm_samples < 2 or args.edit_samples < 2:
        parser.error("both sample counts must be at least two")
    if args.macos_guardian and platform.system() != "Darwin":
        parser.error("--macos-guardian requires macOS")
    if args.profile_guardian and not args.macos_guardian:
        parser.error("--profile-guardian requires --macos-guardian")

    real_rustup_home = os.environ.get("RUSTUP_HOME", str(pathlib.Path.home() / ".rustup"))
    with tempfile.TemporaryDirectory(prefix="rgo-bench-") as temporary:
        root = pathlib.Path(temporary)
        project = root / "project"
        (project / "src").mkdir(parents=True)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "rgo_bench"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (project / "src/main.rs").write_text('fn main() { println!("initial"); }\n')
        env = os.environ.copy()
        if args.macos_guardian:
            env["RGO_MACOS_SUPERVISOR_PILOT"] = "1"
        else:
            env.pop("RGO_MACOS_SUPERVISOR_PILOT", None)
        if args.profile_guardian:
            env["RGO_LOG"] = "rgo=debug"
        env.update(
            HOME=str(root / "home"),
            CARGO_HOME=str(root / "home/.cargo"),
            RGO_HOME=str(root / "home/.rgo"),
            RUSTUP_HOME=real_rustup_home,
        )
        pathlib.Path(env["HOME"]).mkdir()
        rgo = str(args.rgo_bin.resolve())
        # Keep the rustup proxy's cargo basename; resolving a proxy symlink to
        # rustup itself can change how rustup interprets this invocation.
        cargo = str(args.real_cargo)
        _, cargo_version = command([cargo, "--version"], project, env)
        _, rustc_version = command(["rustc", "--version"], project, env)
        _, rgo_version = command([rgo, "--version"], project, env)
        _, source_commit = command(["git", "-C", str(REPO), "rev-parse", "HEAD"], project, env)
        _, source_changes = command(["git", "-C", str(REPO), "status", "--porcelain", "--untracked-files=no"], project, env)
        command([rgo, "setup", "--supervised", "--real-cargo", cargo, "--no-service"], project, env)
        shim = pathlib.Path(env["CARGO_HOME"]) / "rgo/shims/cargo"
        env.pop("RGO_HOME")  # Include fresh-process Cargo-home pointer discovery.
        plain = [cargo, "build", "--offline"]
        managed = [str(shim), "build", "--offline"]
        command(plain, project, env)
        command(managed, project, env)
        for _ in range(4):
            command(plain, project, env)
            command(managed, project, env)
        timings = {"warm_noop": [], "edit_build": []} if args.profile_guardian else None
        warm = paired_samples(plain, managed, project, env, args.warm_samples, False,
                              timings["warm_noop"] if timings else None)
        edit = paired_samples(plain, managed, project, env, args.edit_samples, True,
                              timings["edit_build"] if timings else None)
        result = {
            "platform": platform.platform(),
            "cargo": cargo_version,
            "rustc": rustc_version,
            "rgo": rgo_version,
            "source_commit": source_commit,
            "source_dirty": bool(source_changes),
            "percentile_method": "nearest_rank",
            "rgo_binary_sha256": sha256(pathlib.Path(rgo)),
            "wrapper_binary_sha256": sha256(pathlib.Path(rgo).with_name("rgo-rustc-wrapper")),
            "cache_enabled": False,
            "automatic_gc_enabled": False,
            "macos_supervisor_pilot": args.macos_guardian,
            "warm_noop": warm,
            "edit_build": edit,
        }
        if timings:
            if any(not sample["stages"] for group in timings.values() for sample in group):
                raise RuntimeError("guardian binary did not emit debug timing stages")
            result["guardian_timings"] = timings
        text = json.dumps(result, indent=2) + "\n"
        if args.output:
            args.output.write_text(text)
        print(text, end="")


if __name__ == "__main__":
    main()
