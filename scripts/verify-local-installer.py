#!/usr/bin/env python3
"""Exercise the Unix installer with a local bundle and an isolated Cargo home."""

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
MACHINE = platform.machine().lower()
TARGET = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Darwin", "aarch64"): "aarch64-apple-darwin",
    ("Darwin", "x86_64"): "x86_64-apple-darwin",
    ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
}[(platform.system(), MACHINE)]
def run(command: list[str], *, env: dict[str, str], cwd: Path | None = None) -> None:
    subprocess.run(command, env=env, cwd=cwd, check=True)


def main() -> None:
    supervised = sys.argv[1:] == ["--supervised"]
    if sys.argv[1:] and not supervised:
        raise ValueError("expected optional --supervised")
    cli_version = subprocess.check_output([ROOT / "target" / "debug" / "rgo", "--version"], text=True).strip()
    if not cli_version.startswith("rgo "):
        raise ValueError(f"unexpected CLI version: {cli_version}")
    version = f"v{cli_version.removeprefix('rgo ')}"
    name = f"rgo-{version}-{TARGET}"
    with tempfile.TemporaryDirectory(prefix="rgo installer ü ") as temporary:
        root = Path(temporary)
        archive = root / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            for filename in ("rgo", "rgo-rustc-wrapper"):
                bundle.add(ROOT / "target" / "debug" / filename, arcname=f"{name}/{filename}")

        home = root / "home"
        home.mkdir()
        cargo_home = home / ".cargo"
        cargo_home.mkdir()
        original_config = "[net]\noffline = true\n"
        (cargo_home / "config.toml").write_text(original_config)
        rgo_home = home / ".rgo"
        environment = os.environ.copy()
        environment.update(
            HOME=str(home),
            CARGO_HOME=str(cargo_home),
            RGO_HOME=str(rgo_home),
            RUSTUP_HOME=os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
            RUSTUP_TOOLCHAIN="stable",
        )
        shell = os.environ.get("RGO_INSTALLER_TEST_SHELL") or shutil.which("zsh") or shutil.which("bash")
        if supervised and shell is None:
            raise ValueError("supervised installer probe requires zsh or bash")
        if supervised and Path(shell).name not in {"zsh", "bash"}:
            raise ValueError("supervised installer probe shell must be zsh or bash")
        if supervised:
            environment["SHELL"] = shell
            original_profile = home / (".zprofile" if Path(shell).name == "zsh" else ".bash_profile")
            original_profile_contents = "# existing profile without a final newline"
            original_profile.write_text(original_profile_contents)
        def fresh_shell(
            command: str, *, login: bool, cwd: Path | None = None, path: str | None = None
        ) -> subprocess.CompletedProcess[str]:
            fresh = environment.copy()
            fresh["PATH"] = path or os.environ["PATH"]
            fresh.pop("RGO_HOME", None)
            return subprocess.run(
                [shell, "-lic" if login else "-ic", command],
                env=fresh, cwd=cwd, text=True, capture_output=True,
            )
        installer = [
            sys.executable,
            str(ROOT / "scripts" / "install-unix.py"),
            "--archive",
            str(archive),
            "--version",
            version,
            "--sha256",
            hashlib.sha256(archive.read_bytes()).hexdigest(),
            "--development-bundle",
            "--cargo-home",
            str(cargo_home),
            "--rgo-home",
            str(rgo_home),
            "--no-service",
        ]
        if supervised:
            installer.append("--supervised")
            service_mode = subprocess.run(
                [argument for argument in installer if argument != "--no-service"],
                env=environment, text=True, capture_output=True,
            )
            assert service_mode.returncode != 0
            assert "requires --no-service" in service_mode.stderr
        first_mid_setup = environment.copy()
        first_mid_setup["RGO_SETUP_TEST_EXIT_AFTER_RECORD"] = "1"
        first_mid_result = subprocess.run(installer, env=first_mid_setup, text=True, capture_output=True)
        assert first_mid_result.returncode == 88, first_mid_result.stderr
        first_journal = cargo_home / "rgo" / "installer-transaction.json"
        assert json.loads(first_journal.read_text())["phase"] == "prepared"
        assert (cargo_home / ".rgo-install.json").exists()
        if not supervised:
            pointer = cargo_home / ".rgo-home"
            planned_pointer = pointer.read_bytes()
            pointer.write_text("user-edit\n")
            changed = subprocess.run(installer, env=environment, text=True, capture_output=True)
            assert changed.returncode != 0
            assert "activation changed during upgrade" in changed.stderr, changed.stderr
            assert pointer.read_text() == "user-edit\n"
            assert first_journal.exists()
            pointer.write_bytes(planned_pointer)

        first_interrupt = environment.copy()
        first_interrupt["RGO_INSTALLER_TEST_EXIT_AFTER_SETUP_DONE"] = "1"
        first_result = subprocess.run(installer, env=first_interrupt, text=True, capture_output=True)
        assert first_result.returncode == 86, first_result.stderr
        assert json.loads(first_journal.read_text())["first_install"] is True
        if supervised:
            first_profile_interrupt = environment.copy()
            first_profile_interrupt["RGO_INSTALLER_TEST_EXIT_AFTER_FIRST_PROFILE"] = "1"
            interrupted_profile = subprocess.run(installer, env=first_profile_interrupt, text=True, capture_output=True)
            assert interrupted_profile.returncode == 92, interrupted_profile.stderr
            shell_state = cargo_home / "rgo/shell-activation.json"
            partial_profiles = [Path(item["path"]) for item in json.loads(shell_state.read_text())["profiles"]]
            assert partial_profiles[0].read_text().count("# >>> rgo supervised Cargo ") == 1
            assert not partial_profiles[1].exists()
        run(installer, env=environment)
        assert not first_journal.exists()
        if supervised:
            second_home = root / "second home"
            second_cargo_home = second_home / ".cargo"
            second_cargo_home.mkdir(parents=True)
            second_rgo_home = second_home / ".rgo"
            second_environment = environment.copy()
            second_environment.update(
                HOME=str(second_home),
                CARGO_HOME=str(second_cargo_home),
                RGO_HOME=str(second_rgo_home),
                PATH=f"{cargo_home / 'rgo/shims'}{os.pathsep}{os.environ['PATH']}",
            )
            second_installer = installer.copy()
            second_installer[second_installer.index("--cargo-home") + 1] = str(second_cargo_home)
            second_installer[second_installer.index("--rgo-home") + 1] = str(second_rgo_home)
            foreign_proxy = subprocess.run(
                [*second_installer, "--real-cargo", str(cargo_home / "rgo/shims/cargo")],
                env=second_environment, text=True, capture_output=True,
            )
            assert foreign_proxy.returncode != 0
            assert "--real-cargo must identify" in foreign_proxy.stderr
            assert not (second_cargo_home / ".rgo-install.json").exists()
            run(second_installer, env=second_environment)
            first_record = json.loads((cargo_home / ".rgo-install.json").read_text())
            second_record = json.loads((second_cargo_home / ".rgo-install.json").read_text())
            assert Path(second_record["supervised_cargo"]["real_cargo"]).resolve() == Path(
                first_record["supervised_cargo"]["real_cargo"]
            ).resolve()
            assert Path(second_record["supervised_cargo"]["real_cargo"]).resolve() != (
                cargo_home / "rgo/shims/cargo"
            ).resolve()
            run(
                [
                    sys.executable,
                    str(ROOT / "scripts" / "install-unix.py"),
                    "--uninstall",
                    "--cargo-home",
                    str(second_cargo_home),
                    "--development-bundle",
                ],
                env=second_environment,
            )
            assert (cargo_home / "rgo/shims/cargo").is_file()
            current_path_edit = 'case "$PATH" in "$rgo_shim_dir"|"$rgo_shim_dir":*) ;; *) export PATH="$rgo_shim_dir:$PATH" ;; esac'
            older_path_edit = 'case ":$PATH:" in *":$rgo_shim_dir:"*) ;; *) export PATH="$rgo_shim_dir:$PATH" ;; esac'
            for login in (False, True):
                launched = fresh_shell("command -v cargo", login=login)
                assert launched.returncode == 0, launched.stderr
                assert launched.stdout.strip().splitlines()[-1] == str(cargo_home / "rgo/shims/cargo")
                late_shim = f"{os.environ['PATH']}{os.pathsep}{cargo_home / 'rgo/shims'}"
                launched = fresh_shell("command -v cargo", login=login, path=late_shim)
                assert launched.returncode == 0, launched.stderr
                assert launched.stdout.strip().splitlines()[-1] == str(cargo_home / "rgo/shims/cargo")
            recursive = subprocess.run(
                [*installer, "--real-cargo", str(cargo_home / "rgo/shims/cargo")],
                env=environment, text=True, capture_output=True,
            )
            assert recursive.returncode != 0
            assert "--real-cargo must identify" in recursive.stderr
            shell_state = cargo_home / "rgo/shell-activation.json"
            profiles = [Path(item["path"]) for item in json.loads(shell_state.read_text())["profiles"]]
            for profile in profiles:
                assert profile.read_text().count("# >>> rgo supervised Cargo ") == 1
            profiles[0].write_text(
                (profiles[0].read_text() + "# user edit after install\n").replace(
                    current_path_edit, older_path_edit, 1
                )
            )
            assert older_path_edit in profiles[0].read_text()
            environment["PATH"] = f"{cargo_home / 'rgo' / 'shims'}{os.pathsep}{environment['PATH']}"
        run(installer, env=environment)
        if supervised:
            for profile in profiles:
                assert profile.read_text().count("# >>> rgo supervised Cargo ") == 1
            assert current_path_edit in profiles[0].read_text()
            assert older_path_edit not in profiles[0].read_text()

        active = json.loads((cargo_home / ".rgo-install.json").read_text())
        missing_name = "rgo" if supervised else "rgo-rustc-wrapper"
        missing_binary = Path(active["rgo_binary"]).parent / missing_name
        missing_binary.unlink()
        if supervised:
            fallback = fresh_shell("cargo --version", login=True)
            assert fallback.returncode == 0, fallback.stderr
            assert "rgo: launcher unavailable; using recorded Cargo" in fallback.stderr
        refused = subprocess.run(installer, env=environment, text=True, capture_output=True)
        assert refused.returncode != 0, "ordinary reinstall replaced a damaged owned version"
        assert "differs from the verified archive" in refused.stderr or "unexpected contents" in refused.stderr
        run([*installer, "--repair"], env=environment)
        assert missing_binary.is_file()
        pointer = cargo_home / ".rgo-home"
        expected_pointer = f"{rgo_home}\n"
        pointer.write_text("damaged-pointer\n")
        run([*installer, "--repair"], env=environment)
        assert pointer.read_text() == expected_pointer
        other_root = root / "another-rgo-home"
        pointer.write_text(f"{other_root}\n")
        refused_pointer = subprocess.run(
            [*installer, "--repair"], env=environment, text=True, capture_output=True,
        )
        assert refused_pointer.returncode != 0
        assert "different absolute storage root" in refused_pointer.stderr
        assert pointer.read_text() == f"{other_root}\n"
        pointer.write_text(expected_pointer)
        if not supervised:
            pair = [Path(active["rgo_binary"]).parent / name for name in ("rgo", "rgo-rustc-wrapper")]
            for member in pair:
                member.unlink()
            interrupted_repair_env = environment.copy()
            interrupted_repair_env["RGO_INSTALLER_TEST_EXIT_DURING_REPAIR"] = "1"
            interrupted_repair = subprocess.run(
                [*installer, "--repair"], env=interrupted_repair_env,
                text=True, capture_output=True,
            )
            assert interrupted_repair.returncode == 89, interrupted_repair.stderr
            assert sum(member.is_file() for member in pair) == 1
            run([*installer, "--repair"], env=environment)
            assert all(member.is_file() for member in pair)

        # Simulate an older versioned install with the same protocol. The
        # native branch also forces setup's activation probe to fail after replacement.
        active = json.loads((cargo_home / ".rgo-install.json").read_text())
        old_dir = cargo_home / "rgo" / "versions" / "older-fixture"
        old_dir.mkdir()
        for name in ("rgo", "rgo-rustc-wrapper"):
            shutil.copy2(Path(active["rgo_binary"]).parent / name, old_dir / name)
        old_cli = old_dir / "rgo"
        old_setup = [str(old_cli), "setup", "--no-service"]
        if supervised:
            old_setup.extend(
                ["--supervised", "--real-cargo", active["supervised_cargo"]["real_cargo"]]
            )
        run(old_setup, env=environment)
        for name in ("rgo", "rgo-rustc-wrapper"):
            link = cargo_home / "bin" / name
            link.unlink()
            link.symlink_to(old_dir / name)
        state_path = cargo_home / "rgo" / "installer-state.json"
        state = json.loads(state_path.read_text())
        state["rgo_binary"] = str(old_cli.resolve())
        state_path.write_text(json.dumps(state))

        if not supervised:
            fake_bin = root / "version-only-cargo"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo"
            fake_cargo.write_text(
                '#!/bin/sh\n'
                'if [ "$1" = "--version" ]; then\n'
                '  echo "cargo 1.98.0 (installer fixture)"\n'
                '  exit 0\n'
                'fi\n'
                'exit 1\n'
            )
            fake_cargo.chmod(0o755)
            version_only_cargo = environment.copy()
            version_only_cargo["PATH"] = str(fake_bin)
            failed = subprocess.run(installer, env=version_only_cargo, text=True, capture_output=True)
            assert failed.returncode != 0, "upgrade unexpectedly verified with a non-building Cargo"
            assert "plain Cargo activation could not be verified" in failed.stderr, failed.stderr
            assert "prior Cargo settings were restored" in failed.stderr, failed.stderr
            rolled_back = json.loads((cargo_home / ".rgo-install.json").read_text())
            assert Path(rolled_back["rgo_binary"]).resolve() == old_cli.resolve()
            assert (cargo_home / "bin" / "rgo").resolve() == old_cli.resolve()
            assert str(old_dir / "rgo-rustc-wrapper") in (cargo_home / "config.toml").read_text()
            run([str(old_cli), "doctor", "--verify"], env=environment)

        journal_path = cargo_home / "rgo" / "installer-transaction.json"
        mid_setup_env = environment.copy()
        mid_setup_env["RGO_SETUP_TEST_EXIT_AFTER_RECORD"] = "1"
        mid_setup = subprocess.run(installer, env=mid_setup_env, text=True, capture_output=True)
        assert mid_setup.returncode == 88, mid_setup.stderr
        assert json.loads(journal_path.read_text())["phase"] == "prepared"

        prepared_env = environment.copy()
        prepared_env["RGO_INSTALLER_TEST_EXIT_AFTER_PREPARED"] = "1"
        prepared = subprocess.run(installer, env=prepared_env, text=True, capture_output=True)
        assert prepared.returncode == 85, prepared.stderr
        assert json.loads(journal_path.read_text())["phase"] == "prepared"
        assert Path(json.loads((cargo_home / ".rgo-install.json").read_text())["rgo_binary"]).resolve() == old_cli.resolve()

        interrupted_env = environment.copy()
        interrupted_env["RGO_INSTALLER_TEST_EXIT_AFTER_SETUP_DONE"] = "1"
        interrupted = subprocess.run(installer, env=interrupted_env, text=True, capture_output=True)
        assert interrupted.returncode == 86, interrupted.stderr
        assert json.loads(journal_path.read_text())["phase"] == "setup_done"
        assert json.loads((cargo_home / "rgo" / "installer-state.json").read_text())["rgo_binary"] == str(old_cli.resolve())
        assert Path(json.loads((cargo_home / ".rgo-install.json").read_text())["rgo_binary"]).resolve() != old_cli.resolve()

        committed_env = environment.copy()
        committed_env["RGO_INSTALLER_TEST_EXIT_AFTER_COMMITTED"] = "1"
        committed = subprocess.run(installer, env=committed_env, text=True, capture_output=True)
        assert committed.returncode == 87, committed.stderr
        assert json.loads(journal_path.read_text())["phase"] == "committed"

        run(installer, env=environment)
        assert not journal_path.exists(), "recovered upgrade left a transaction journal"
        upgraded = json.loads((cargo_home / ".rgo-install.json").read_text())
        assert Path(upgraded["rgo_binary"]).resolve() != old_cli.resolve()
        if supervised:
            shim = (cargo_home / "rgo/shims/cargo").read_text()
            assert upgraded["rgo_binary"] in shim
            assert str(old_cli) not in shim

        record = json.loads((cargo_home / ".rgo-install.json").read_text())
        assert Path(record["rgo_binary"]).resolve().is_relative_to(cargo_home.resolve())
        assert (cargo_home / "bin" / "rgo").is_symlink()
        assert (cargo_home / "bin" / "rgo-rustc-wrapper").is_symlink()
        if supervised:
            assert (cargo_home / "config.toml").read_text() == original_config

        project = root / "ordinary project"
        (project / "src").mkdir(parents=True)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "rgo-installer-probe"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (project / "src" / "main.rs").write_text('fn main() { println!("ok"); }\n')
        if supervised:
            launched = fresh_shell("cargo build --offline", login=True, cwd=project)
            assert launched.returncode == 0, launched.stderr
        plain_cargo_environment = environment.copy()
        plain_cargo_environment.pop("RGO_HOME", None)
        run(["cargo", "build", "--offline"], env=plain_cargo_environment, cwd=project)
        assert list((rgo_home / "builds").glob("*/*/.rgo-context.json"))

        installed_version = Path(record["rgo_binary"]).parent
        uninstaller = [
            sys.executable,
            str(ROOT / "scripts" / "install-unix.py"),
            "--uninstall",
            "--cargo-home",
            str(cargo_home),
            "--development-bundle",
        ]
        if supervised:
            owned_profile = profiles[0].read_text()
            profiles[0].write_text(owned_profile.replace("rgo_shim_dir=", "rgo_shim_dir_changed=", 1))
            edited_profile = subprocess.run(uninstaller, env=environment, text=True, capture_output=True)
            assert edited_profile.returncode != 0
            assert "owned shell activation changed" in edited_profile.stderr
            assert (cargo_home / ".rgo-install.json").exists()
            profiles[0].write_text(owned_profile.replace(current_path_edit, older_path_edit, 1))
        if not supervised:
            after_undo = environment.copy()
            after_undo["RGO_INSTALLER_TEST_EXIT_AFTER_UNDO"] = "1"
            interrupted_undo = subprocess.run(uninstaller, env=after_undo, text=True, capture_output=True)
            assert interrupted_undo.returncode == 90, interrupted_undo.stderr
            assert not (cargo_home / ".rgo-install.json").exists()
            pending_install = subprocess.run(installer, env=environment, text=True, capture_output=True)
            assert pending_install.returncode != 0
            assert "an uninstall is pending" in pending_install.stderr, pending_install.stderr
            installer_state = cargo_home / "rgo/installer-state.json"
            owned_state = installer_state.read_bytes()
            installer_state.write_text('{"user_edit": true}\n')
            edited_uninstall = subprocess.run(uninstaller, env=environment, text=True, capture_output=True)
            assert edited_uninstall.returncode != 0
            assert "installer state changed during uninstall" in edited_uninstall.stderr
            assert (cargo_home / "bin/rgo").is_symlink()
            installer_state.write_bytes(owned_state)
            after_one_link = environment.copy()
            after_one_link["RGO_INSTALLER_TEST_EXIT_AFTER_FIRST_UNLINK"] = "1"
            interrupted_links = subprocess.run(uninstaller, env=after_one_link, text=True, capture_output=True)
            assert interrupted_links.returncode == 91, interrupted_links.stderr
        run(uninstaller, env=environment)
        run(uninstaller, env=environment)
        assert not (cargo_home / ".rgo-install.json").exists()
        assert (cargo_home / "config.toml").read_text() == original_config
        assert not (cargo_home / "rgo" / "installer-state.json").exists()
        assert not (cargo_home / "bin" / "rgo").exists()
        assert not (cargo_home / "bin" / "rgo-rustc-wrapper").exists()
        assert installed_version.is_dir(), "versioned binaries should survive in-flight Cargo use"
        if supervised:
            assert not (cargo_home / "rgo" / "shims" / "cargo").exists()
            assert not (cargo_home / "rgo" / "shell-activation.json").exists()
            assert profiles[0].read_text() == original_profile_contents + "\n# user edit after install\n"
            for profile in profiles[1:]:
                assert not profile.exists()
            for login in (False, True):
                launched = fresh_shell("command -v cargo", login=login)
                assert launched.returncode == 0, launched.stderr
                assert launched.stdout.strip().splitlines()[-1] != str(cargo_home / "rgo/shims/cargo")
        run(["cargo", "build", "--offline"], env=plain_cargo_environment, cwd=project)
        assert (project / "target").exists()
        run(installer, env=environment)
        run(uninstaller, env=environment)
        assert not (cargo_home / ".rgo-install.json").exists()
        print("isolated Unix installer round trip passed")


if __name__ == "__main__":
    main()
