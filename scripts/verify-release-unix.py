#!/usr/bin/env python3
"""Smoke-test optimized Unix release binaries through a private installation."""

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
TARGET = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Darwin", "aarch64"): "aarch64-apple-darwin",
    ("Darwin", "x86_64"): "x86_64-apple-darwin",
    ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
}[(platform.system(), platform.machine().lower())]


def check(command: list[str], *, env: dict[str, str], cwd: Path | None = None) -> None:
    subprocess.run(command, env=env, cwd=cwd, check=True, timeout=120)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: verify-release-unix.py BINARY_DIR")
    binaries = Path(sys.argv[1]).resolve(strict=True)
    cli = binaries / "rgo"
    wrapper = binaries / "rgo-rustc-wrapper"
    version = subprocess.check_output([cli, "--version"], text=True).strip().removeprefix("rgo ")
    if not version or not wrapper.is_file():
        raise RuntimeError("release binaries are missing")
    tag = f"v{version}"
    name = f"rgo-{tag}-{TARGET}"
    real_cargo = shutil.which("cargo")
    shell = shutil.which("zsh") or shutil.which("bash")
    if not real_cargo or not shell:
        raise RuntimeError("Cargo and a supported shell are required")

    with tempfile.TemporaryDirectory(prefix="rgor-", dir="/tmp") as temporary:
        root = Path(temporary)
        archive = root / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            for source in (cli, wrapper):
                bundle.add(source, arcname=f"{name}/{source.name}")
            for filename in ("README.md", "doc.md", "LICENSE-MIT", "LICENSE-APACHE"):
                bundle.add(ROOT / filename, arcname=f"{name}/{filename}")
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()

        for supervised in (False, True):
            mode = "supervised" if supervised else "native"
            home = root / mode
            cargo_home = home / ".cargo"
            rgo_home = home / ".rgo"
            cargo_home.mkdir(parents=True)
            original_config = "[net]\noffline = true\n"
            (cargo_home / "config.toml").write_text(original_config)
            env = os.environ.copy()
            env.update(
                HOME=str(home), CARGO_HOME=str(cargo_home), RGO_HOME=str(rgo_home),
                RUSTUP_HOME=os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
                RUSTUP_TOOLCHAIN="stable", SHELL=shell,
            )
            installer = [
                sys.executable, str(ROOT / "scripts/install-unix.py"),
                "--archive", str(archive), "--version", tag, "--sha256", digest,
                "--development-bundle", "--cargo-home", str(cargo_home),
                "--rgo-home", str(rgo_home), "--no-service",
            ]
            if supervised:
                installer.extend(("--supervised", "--real-cargo", real_cargo))
            check(installer, env=env)

            record = json.loads((cargo_home / ".rgo-install.json").read_text())
            project = home / "project"
            (project / "src").mkdir(parents=True)
            (project / "Cargo.toml").write_text(
                '[package]\nname = "rgo_release_probe"\nversion = "0.1.0"\nedition = "2021"\n'
            )
            (project / "src/main.rs").write_text('fn main() { println!("release"); }\n')
            build_env = env.copy()
            build_env.pop("RGO_HOME")
            if supervised:
                shim = Path(record["supervised_cargo"]["shim_path"])
                build_env["PATH"] = f"{shim.parent}{os.pathsep}{build_env['PATH']}"
            check(["cargo", "build", "--offline"], env=build_env, cwd=project)
            if not (project / "target/debug/rgo_release_probe").is_file():
                raise RuntimeError(f"{mode}: Cargo did not preserve the requested executable")
            if not list((rgo_home / "builds").glob("*/*/.rgo-context.json")):
                raise RuntimeError(f"{mode}: unchanged Cargo did not activate managed storage")

            check([
                sys.executable, str(ROOT / "scripts/install-unix.py"), "--uninstall",
                "--cargo-home", str(cargo_home), "--development-bundle",
            ], env=env)
            if (cargo_home / ".rgo-install.json").exists():
                raise RuntimeError(f"{mode}: uninstall left Cargo activation behind")
            if (cargo_home / "config.toml").read_text() != original_config:
                raise RuntimeError(f"{mode}: uninstall changed the prior Cargo config")
            print(f"{TARGET}: optimized {mode} install, plain Cargo build, and undo passed")


if __name__ == "__main__":
    main()
