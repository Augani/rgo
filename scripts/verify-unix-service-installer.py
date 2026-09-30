#!/usr/bin/env python3
"""Exercise supervised Unix installation with a real user service in a private home."""

import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str], environment: dict[str, str], cwd: Path | None = None) -> None:
    subprocess.run(command, env=environment, cwd=cwd, check=True, timeout=120)


def main() -> None:
    if platform.system() != "Darwin":
        raise RuntimeError("this service probe requires macOS launchd")
    target = "aarch64-apple-darwin" if platform.machine().lower() in {"arm64", "aarch64"} else "x86_64-apple-darwin"
    version = "v" + subprocess.check_output([ROOT / "target/debug/rgo", "--version"], text=True).split()[1]
    root = Path(tempfile.mkdtemp(prefix="rgo-svc-", dir="/tmp"))
    home = root / "h"
    cargo_home = home / "c"
    rgo_home = home / "r"
    cargo_home.mkdir(parents=True)
    (cargo_home / "config.toml").write_text("[net]\noffline = true\n")
    name = f"rgo-{version}-{target}"
    archive = root / f"{name}.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        for binary in ("rgo", "rgo-rustc-wrapper"):
            bundle.add(ROOT / "target/debug" / binary, arcname=f"{name}/{binary}")
    environment = os.environ.copy()
    environment.update(
        HOME=str(home),
        CARGO_HOME=str(cargo_home),
        RGO_HOME=str(rgo_home),
        RUSTUP_HOME=os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
        RUSTUP_TOOLCHAIN="stable",
        SHELL="/bin/zsh",
    )
    installer = [
        sys.executable, str(ROOT / "scripts/install-unix.py"),
        "--archive", str(archive), "--version", version,
        "--sha256", hashlib.sha256(archive.read_bytes()).hexdigest(),
        "--development-bundle", "--cargo-home", str(cargo_home),
        "--rgo-home", str(rgo_home), "--supervised",
    ]
    uninstaller = [
        sys.executable, str(ROOT / "scripts/install-unix.py"),
        "--uninstall", "--cargo-home", str(cargo_home), "--development-bundle",
    ]
    completed = False
    try:
        run(installer, environment)
        record = json.loads((cargo_home / ".rgo-install.json").read_text())
        assert record["supervised_cargo"] is not None
        assert (cargo_home / "rgo/installer-state.json").exists()
        project = root / "project"
        (project / "src").mkdir(parents=True)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "rgo-service-probe"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (project / "src/main.rs").write_text("fn main() {}\n")
        fresh = environment.copy()
        fresh.pop("RGO_HOME")
        fresh["PATH"] = f"{cargo_home / 'rgo/shims'}{os.pathsep}{fresh['PATH']}"
        run(["cargo", "build", "--offline"], fresh, project)
        assert list((rgo_home / "builds").glob("*/*/.rgo-context.json"))
        run(uninstaller, environment)
        assert not (cargo_home / ".rgo-install.json").exists()
        assert not (cargo_home / "rgo/installer-state.json").exists()
        assert not (cargo_home / "rgo/shims/cargo").exists()
        assert (cargo_home / "config.toml").read_text() == "[net]\noffline = true\n"
        completed = True
        print("private supervised service install, Cargo build, and uninstall passed")
    finally:
        if completed:
            shutil.rmtree(root)
        else:
            print(f"retained failed private service probe at {root}", file=sys.stderr)


if __name__ == "__main__":
    main()
