#!/usr/bin/env python3
"""Verify source packages without publishing or changing the developer's Cargo setup.

Cargo cannot resolve unpublished workspace crates from crates.io. Package the leaf
crates first, then patch each dependent package to the extracted contents of
those .crate archives. Finally install the two packaged binaries into a
temporary root and exercise plain Cargo in a private HOME/CARGO_HOME/RGO_HOME.
"""

import json
import os
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
LEAVES = (
    "rgo-protocol",
    "rgo-cas",
    "rgo-key",
    "rgo-materialize",
    "rgo-remote",
)
PACKAGES = (*LEAVES, "rgo-core", "rgo-rustc-wrapper", "rgo-storage")
DEPENDENCIES = {
    "rgo-core": ("rgo-protocol", "rgo-cas", "rgo-remote"),
    "rgo-rustc-wrapper": ("rgo-protocol", "rgo-cas", "rgo-key", "rgo-materialize"),
    "rgo-storage": ("rgo-core", "rgo-protocol", "rgo-cas", "rgo-materialize", "rgo-remote"),
}


def run(*args: str, cwd: Path = ROOT, env: dict[str, str] | None = None) -> None:
    subprocess.run(args, cwd=cwd, env=env, check=True)


def metadata() -> dict:
    result = subprocess.run(
        ("cargo", "metadata", "--no-deps", "--format-version", "1"),
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def patches(names: tuple[str, ...], extracted: dict[str, Path]) -> list[str]:
    args: list[str] = []
    for name in names:
        # A JSON string is also a valid TOML basic string for filesystem paths.
        path = json.dumps(str(extracted[name]), ensure_ascii=False)
        args.extend(("--config", f"patch.crates-io.{name}.path={path}"))
    return args


def verify() -> None:
    manifest = metadata()
    versions = {package["name"]: package["version"] for package in manifest["packages"]}
    version = versions["rgo-storage"]
    if any(versions[name] != version for name in PACKAGES):
        raise RuntimeError("source packages must all have the same version")
    package_root = Path(manifest["target_directory"]) / "package"
    extracted = {name: package_root / f"{name}-{version}" for name in PACKAGES}

    for name in PACKAGES:
        run("cargo", "package", "--allow-dirty", "-p", name, *patches(DEPENDENCIES.get(name, ()), extracted))
        if not extracted[name].is_dir():
            raise RuntimeError(f"Cargo did not extract packaged {name} at {extracted[name]}")
        if not (package_root / f"{name}-{version}.crate").is_file():
            raise RuntimeError(f"Cargo did not create the {name} archive")
        for license_name in ("LICENSE-MIT", "LICENSE-APACHE"):
            if not (extracted[name] / license_name).is_file():
                raise RuntimeError(f"{name} package is missing {license_name}")

    with tempfile.TemporaryDirectory(prefix="rgo-source-preflight-") as temporary:
        private = Path(temporary)
        install_root = private / "installed binaries ü"
        install_root.mkdir()
        for name in ("rgo-storage", "rgo-rustc-wrapper"):
            run(
                "cargo",
                "install",
                "--locked",
                "--offline",
                "--debug",
                "--path",
                str(extracted[name]),
                "--root",
                str(install_root),
                *patches(DEPENDENCIES[name], extracted),
            )

        suffix = ".exe" if os.name == "nt" else ""
        cli = install_root / "bin" / f"rgo{suffix}"
        wrapper = install_root / "bin" / f"rgo-rustc-wrapper{suffix}"
        if not cli.is_file() or not wrapper.is_file():
            raise RuntimeError("the packaged installation did not produce both binaries")

        home = private / "private home ü"
        cargo_home = home / ".cargo"
        rgo_home = home / ".rgo"
        project = private / "plain cargo project"
        for directory in (cargo_home, rgo_home, project / "src"):
            directory.mkdir(parents=True)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "rgo_package_probe"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (project / "src" / "main.rs").write_text('fn main() { println!("rgo"); }\n')
        (project / "build.rs").write_text("fn main() {}\n")
        sandbox = os.environ.copy()
        rustup_home = subprocess.run(
            ("rustup", "show", "home"),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        sandbox.update(
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
        ):
            sandbox.pop(variable, None)
        run(str(cli), "setup", "--no-service", env=sandbox)
        run("cargo", "build", "--offline", "--manifest-path", str(project / "Cargo.toml"), env=sandbox)
        if not (project / "target" / "debug" / f"rgo_package_probe{suffix}").is_file():
            raise RuntimeError("plain Cargo did not leave the final binary in target/debug")
        doctor = subprocess.run(
            (str(cli), "doctor", "--verify", "--json"),
            cwd=project,
            env=sandbox,
            check=True,
            capture_output=True,
            text=True,
        )
        if json.loads(doctor.stdout)["activation_verified"] is not True:
            raise RuntimeError("the packaged pair did not activate plain Cargo")
        run(str(cli), "setup", "--undo", "--no-service", env=sandbox)
        config = (cargo_home / "config.toml").read_text()
        if "rgo managed" in config or "rustc-wrapper" in config or "build-dir" in config:
            raise RuntimeError("undo left managed Cargo settings in the private home")

    print(f"source packages v{version}: packaged, installed, activated, and undone")


if __name__ == "__main__":
    verify()
