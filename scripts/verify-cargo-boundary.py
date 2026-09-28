#!/usr/bin/env python3
"""Exercise native activation on both sides of Cargo's build-dir stabilization.

The released rgo binaries are built with the CI host's current toolchain. All
Cargo invocations below use private homes and the exact toolchain under test.
"""

import json
import os
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
RGO = ROOT / "target" / "debug" / ("rgo.exe" if os.name == "nt" else "rgo")
ORIGINAL_CONFIG = b"[net]\noffline = true\n"


def run(args: list[str], *, cwd: Path, env: dict[str, str], succeeds: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True)
    if (result.returncode == 0) != succeeds:
        raise RuntimeError(
            f"{' '.join(args)} returned {result.returncode}; expected "
            f"{'success' if succeeds else 'failure'}:\n{result.stdout}\n{result.stderr}"
        )
    return result


def build_out_dirs(project: Path, env: dict[str, str]) -> list[Path]:
    build = run(["cargo", "build", "--offline", "--message-format=json"], cwd=project, env=env)
    return [
        Path(message["out_dir"]).resolve()
        for line in build.stdout.splitlines()
        if line.startswith("{")
        for message in [json.loads(line)]
        if message.get("reason") == "build-script-executed"
    ]


def probe(version: str, rustup_home: str) -> None:
    with tempfile.TemporaryDirectory(prefix=f"rgo-cargo-{version}-") as raw:
        root = Path(raw)
        home = root / "home"
        cargo_home = home / ".cargo"
        rgo_home = home / ".rgo"
        project = root / "project"
        (project / "src").mkdir(parents=True)
        cargo_home.mkdir(parents=True)
        config = cargo_home / "config.toml"
        config.write_bytes(ORIGINAL_CONFIG)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "rgo_boundary_probe"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (project / "build.rs").write_text("fn main() {}\n")
        (project / "src" / "main.rs").write_text("fn main() {}\n")

        env = os.environ.copy()
        env.update(
            HOME=str(home),
            USERPROFILE=str(home),
            CARGO_HOME=str(cargo_home),
            RGO_HOME=str(rgo_home),
            RUSTUP_HOME=rustup_home,
            RUSTUP_TOOLCHAIN=version,
        )
        for variable in (
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_BUILD_DIR",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
        ):
            env.pop(variable, None)

        actual = run(["cargo", "--version"], cwd=project, env=env).stdout.strip()
        if not actual.startswith(f"cargo {version} "):
            raise RuntimeError(f"expected Cargo {version}; got {actual!r}")

        if version == "1.90.0":
            rejected = run([str(RGO), "setup", "--no-service"], cwd=project, env=env, succeeds=False)
            if "requires Cargo 1.91 or newer" not in rejected.stderr:
                raise RuntimeError(f"Cargo {version} was rejected for the wrong reason: {rejected.stderr}")
            if config.read_bytes() != ORIGINAL_CONFIG:
                raise RuntimeError("unsupported Cargo activation changed the user config")
            expected_root = (project / "target").resolve()
        else:
            run([str(RGO), "setup", "--no-service"], cwd=project, env=env)
            expected_root = (rgo_home / "builds").resolve()

        out_dirs = build_out_dirs(project, env)
        if not out_dirs or not all(path.is_relative_to(expected_root) for path in out_dirs):
            raise RuntimeError(f"Cargo {version} intermediates were not in {expected_root}: {out_dirs}")
        executable = project / "target" / "debug" / (
            "rgo_boundary_probe.exe" if os.name == "nt" else "rgo_boundary_probe"
        )
        if not executable.is_file():
            raise RuntimeError(f"Cargo {version} did not leave the final executable in target/debug")

        if version == "1.91.0":
            run([str(RGO), "setup", "--undo", "--no-service"], cwd=project, env=env)
            if config.read_bytes() != ORIGINAL_CONFIG:
                raise RuntimeError("undo did not restore the exact original Cargo config")
            local_out_dirs = build_out_dirs(project, env)
            local_root = (project / "target").resolve()
            if not local_out_dirs or not all(path.is_relative_to(local_root) for path in local_out_dirs):
                raise RuntimeError("plain Cargo did not return to local intermediates after undo")


def main() -> None:
    if not RGO.is_file():
        raise RuntimeError("build every workspace binary before running this probe")
    rustup_home = subprocess.run(
        ["rustup", "show", "home"], check=True, capture_output=True, text=True
    ).stdout.strip()
    for version in ("1.90.0", "1.91.0"):
        probe(version, rustup_home)
    print("Cargo 1.90 rejects activation; Cargo 1.91 relocates and reverses with unchanged Cargo commands")


if __name__ == "__main__":
    main()
