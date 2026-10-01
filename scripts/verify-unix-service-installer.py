#!/usr/bin/env python3
"""Exercise supervised Unix installation with a real user service in a private home."""

import hashlib
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str], environment: dict[str, str], cwd: Path | None = None) -> None:
    subprocess.run(command, env=environment, cwd=cwd, check=True, timeout=120)


def verify_launchd_restart(cli: Path, rgo_home: Path, environment: dict[str, str]) -> None:
    pid_file = rgo_home / "state/daemon.pid"
    original = int(pid_file.read_text().strip())
    os.kill(original, signal.SIGKILL)
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        try:
            current = int(pid_file.read_text().strip())
        except (FileNotFoundError, ValueError):
            current = original
        if current != original:
            doctor = subprocess.run(
                [str(cli), "doctor", "--json"], env=environment,
                text=True, capture_output=True, timeout=15,
            )
            if doctor.returncode == 0:
                entries = json.loads(doctor.stdout)["entries"]
                if any(
                    entry["level"] == "ok" and entry["message"].startswith("daemon responds")
                    for entry in entries
                ) and any(
                    entry["message"].startswith(f"daemon pid {current}:")
                    for entry in entries
                ):
                    print(f"launchd restarted private daemon {original} -> {current}")
                    return
        time.sleep(0.5)
    raise RuntimeError("launchd did not restart the private rgo daemon after SIGKILL")


def main() -> None:
    if platform.system() != "Darwin":
        raise RuntimeError("this service probe requires macOS launchd")
    target = "aarch64-apple-darwin" if platform.machine().lower() in {"arm64", "aarch64"} else "x86_64-apple-darwin"
    version = "v" + subprocess.check_output([ROOT / "target/debug/rgo", "--version"], text=True).split()[1]
    root = Path(tempfile.mkdtemp(prefix="rgo svc ü ", dir="/tmp"))
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
        interrupted = environment.copy()
        interrupted["RGO_SETUP_TEST_EXIT_AFTER_RECORD"] = "1"
        first = subprocess.run(installer, env=interrupted, text=True, capture_output=True, timeout=120)
        assert first.returncode == 88, first.stderr
        assert (cargo_home / ".rgo-install.json").exists()
        assert not (cargo_home / "rgo/installer-state.json").exists()
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
        active_cli = Path(record["rgo_binary"])
        verify_launchd_restart(active_cli, rgo_home, fresh)
        active_cli.unlink()
        run([*installer, "--repair"], environment)
        assert active_cli.is_file()

        # Build a distinct version so rollback crosses real binary versions,
        # rather than only switching between paths containing the same build.
        old_version = version.removeprefix("v")
        stem, patch = old_version.rsplit(".", 1)
        new_version = f"{stem}.{int(patch) + 1}"
        new_tag = f"v{new_version}"
        source = root / "upgrade-source"
        source.mkdir()
        shutil.copy2(ROOT / "Cargo.toml", source / "Cargo.toml")
        shutil.copy2(ROOT / "Cargo.lock", source / "Cargo.lock")
        shutil.copytree(ROOT / "crates", source / "crates")
        manifest = source / "Cargo.toml"
        manifest.write_text(manifest.read_text().replace(old_version, new_version))
        build_environment = os.environ.copy()
        build_environment["CARGO_TARGET_DIR"] = str(root / "upgrade-target")
        subprocess.run(
            ["cargo", "build", "--offline", "--manifest-path", str(manifest),
             "-p", "rgo-storage", "-p", "rgo-rustc-wrapper"],
            env=build_environment, check=True, timeout=300,
        )
        upgrade_name = f"rgo-{new_tag}-{target}"
        upgrade_archive = root / f"{upgrade_name}.tar.gz"
        with tarfile.open(upgrade_archive, "w:gz") as bundle:
            for binary in ("rgo", "rgo-rustc-wrapper"):
                bundle.add(
                    root / "upgrade-target/debug" / binary,
                    arcname=f"{upgrade_name}/{binary}",
                )
        upgrade_installer = installer.copy()
        upgrade_installer[upgrade_installer.index("--archive") + 1] = str(upgrade_archive)
        upgrade_installer[upgrade_installer.index("--version") + 1] = new_tag
        upgrade_installer[upgrade_installer.index("--sha256") + 1] = hashlib.sha256(
            upgrade_archive.read_bytes()
        ).hexdigest()

        failed_environment = environment.copy()
        failed_environment["RGO_BYPASS"] = "1"
        failed_upgrade = subprocess.run(
            upgrade_installer, env=failed_environment, text=True, capture_output=True, timeout=120
        )
        assert failed_upgrade.returncode != 0, "bypassed Cargo unexpectedly verified"
        assert Path(json.loads((cargo_home / ".rgo-install.json").read_text())["rgo_binary"]).resolve() == active_cli.resolve()
        assert not (cargo_home / "rgo/installer-transaction.json").exists()
        old_doctor = subprocess.run(
            [str(active_cli), "doctor", "--verify", "--json"],
            env=fresh, text=True, capture_output=True, timeout=120,
        )
        assert old_doctor.returncode == 0, old_doctor.stderr
        assert json.loads(old_doctor.stdout)["activation_verified"] is True

        phases = (
            ("RGO_SETUP_TEST_EXIT_AFTER_RECORD", 88),
            ("RGO_SETUP_TEST_EXIT_AFTER_SERVICE_FILE", 90),
            ("RGO_INSTALLER_TEST_EXIT_AFTER_PREPARED", 85),
            ("RGO_INSTALLER_TEST_EXIT_AFTER_SETUP_DONE", 86),
            ("RGO_INSTALLER_TEST_EXIT_AFTER_COMMITTED", 87),
        )
        for signal, code in phases:
            forced = environment.copy()
            forced[signal] = "1"
            stopped = subprocess.run(upgrade_installer, env=forced, text=True, capture_output=True, timeout=120)
            assert stopped.returncode == code, stopped.stderr
        run(upgrade_installer, environment)
        upgraded = json.loads((cargo_home / ".rgo-install.json").read_text())
        assert upgraded["binary_version"] == new_version
        assert Path(upgraded["rgo_binary"]).resolve() != active_cli.resolve()
        assert not (cargo_home / "rgo/installer-transaction.json").exists()
        run(["cargo", "build", "--offline"], fresh, project)
        run(uninstaller, environment)
        assert not (cargo_home / ".rgo-install.json").exists()
        assert not (cargo_home / "rgo/installer-state.json").exists()
        assert not (cargo_home / "rgo/shims/cargo").exists()
        assert (cargo_home / "config.toml").read_text() == "[net]\noffline = true\n"
        after_setup = environment.copy()
        after_setup["RGO_INSTALLER_TEST_EXIT_AFTER_SETUP_DONE"] = "1"
        second = subprocess.run(installer, env=after_setup, text=True, capture_output=True, timeout=120)
        assert second.returncode == 86, second.stderr
        assert (cargo_home / ".rgo-install.json").exists()
        assert not (cargo_home / "rgo/installer-state.json").exists()
        run(installer, environment)
        assert (cargo_home / "rgo/installer-state.json").exists()
        run(uninstaller, environment)

        after_service_file = environment.copy()
        after_service_file["RGO_SETUP_TEST_EXIT_AFTER_SERVICE_FILE"] = "1"
        third = subprocess.run(
            installer, env=after_service_file, text=True, capture_output=True, timeout=120
        )
        assert third.returncode == 90, third.stderr
        assert (cargo_home / ".rgo-install.json").exists()
        assert not (cargo_home / "rgo/installer-state.json").exists()
        run(installer, environment)
        run(uninstaller, environment)

        native_home = root / "native-home"
        native_cargo_home = native_home / ".cargo"
        native_rgo_home = native_home / ".rgo"
        native_cargo_home.mkdir(parents=True)
        (native_cargo_home / "config.toml").write_text("[net]\noffline = true\n")
        native_environment = environment.copy()
        native_environment.update(
            HOME=str(native_home), CARGO_HOME=str(native_cargo_home),
            RGO_HOME=str(native_rgo_home),
        )
        native_installer = [
            sys.executable, str(ROOT / "scripts/install-unix.py"),
            "--archive", str(archive), "--version", version,
            "--sha256", hashlib.sha256(archive.read_bytes()).hexdigest(),
            "--development-bundle", "--cargo-home", str(native_cargo_home),
            "--rgo-home", str(native_rgo_home),
        ]
        native_upgrade = native_installer.copy()
        native_upgrade[native_upgrade.index("--archive") + 1] = str(upgrade_archive)
        native_upgrade[native_upgrade.index("--version") + 1] = new_tag
        native_upgrade[native_upgrade.index("--sha256") + 1] = hashlib.sha256(
            upgrade_archive.read_bytes()
        ).hexdigest()
        run(native_installer, native_environment)
        run(native_upgrade, native_environment)
        native_record = json.loads((native_cargo_home / ".rgo-install.json").read_text())
        assert native_record["binary_version"] == new_version
        assert native_record["supervised_cargo"] is None
        run([
            sys.executable, str(ROOT / "scripts/install-unix.py"), "--uninstall",
            "--cargo-home", str(native_cargo_home), "--development-bundle",
        ], native_environment)
        assert (native_cargo_home / "config.toml").read_text() == "[net]\noffline = true\n"
        completed = True
        print("private supervised and native service install, upgrade, Cargo build, and uninstall passed")
    finally:
        if completed:
            shutil.rmtree(root)
        else:
            print(f"retained failed private service probe at {root}", file=sys.stderr)


if __name__ == "__main__":
    main()
