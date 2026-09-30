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
            sidecars = list(expected_root.rglob(".rgo-context.json"))
            if len(sidecars) != 1:
                raise RuntimeError(f"expected one attributed Cargo 1.91 build context; got {sidecars}")
            sidecar = sidecars[0]
            context = sidecar.parent
            context_id = str(context.relative_to(expected_root))
            run([str(RGO), "pin", context_id], cwd=project, env=env)
            run(["cargo", "clean", "-p", "rgo_boundary_probe", "--offline"], cwd=project, env=env)
            if not sidecar.is_file():
                raise RuntimeError("Cargo 1.91 clean -p removed rgo's top-level sidecar")
            run(["cargo", "clean", "--offline"], cwd=project, env=env)
            if context.exists():
                raise RuntimeError("Cargo 1.91 full clean retained the managed build context")
            absent_listing = run([str(RGO), "ls"], cwd=project, env=env).stdout
            if context_id not in absent_listing or "(context absent; pin retained)" not in absent_listing:
                raise RuntimeError(f"Cargo 1.91 full clean lost the durable pin:\n{absent_listing}")
            rebuilt_out_dirs = build_out_dirs(project, env)
            if not rebuilt_out_dirs or not all(path.is_relative_to(expected_root) for path in rebuilt_out_dirs):
                raise RuntimeError("Cargo 1.91 rebuild after clean did not use managed storage")
            if not sidecar.is_file():
                raise RuntimeError("Cargo 1.91 rebuild did not restore rgo's attribution sidecar")
            rebuilt_listing = run([str(RGO), "ls"], cwd=project, env=env).stdout
            if not any(context_id in line and "PIN" in line for line in rebuilt_listing.splitlines()):
                raise RuntimeError(f"Cargo 1.91 rebuild lost the durable pin:\n{rebuilt_listing}")
            run([str(RGO), "unpin", context_id], cwd=project, env=env)
            unpinned_listing = run([str(RGO), "ls"], cwd=project, env=env).stdout
            if any(context_id in line and "PIN" in line for line in unpinned_listing.splitlines()):
                raise RuntimeError("explicit unpin after Cargo 1.91 clean did not take effect")

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
    print("Cargo 1.90 rejects activation; Cargo 1.91 relocates, preserves pins through clean and rebuild, and reverses with unchanged Cargo commands")


if __name__ == "__main__":
    main()
