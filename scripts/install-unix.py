#!/usr/bin/env python3
"""Install an attested rgo release bundle without replacing a live binary.

This is the Unix entry point while the public release location is unresolved.
It accepts an already downloaded archive or an explicit GitHub repository/tag.
The development-bundle switch exists only for private, locally built probes.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import fcntl
import hashlib
import json
import os
import platform
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath


MAX_ARCHIVE = 200 * 1024 * 1024
MAX_UNPACKED = 500 * 1024 * 1024
MAX_ACTIVATION_FILE = 4 * 1024 * 1024
MAX_JOURNAL = 128 * 1024 * 1024
ALLOWED_FILES = {
    "rgo",
    "rgo-rustc-wrapper",
    "README.md",
    "doc.md",
    "LICENSE-MIT",
    "LICENSE-APACHE",
}
VERSION = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?\Z")
REPO = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
GH_VERSION = "2.101.0"
# Digests from cli/cli's verified v2.101.0 release checksum manifest. The
# verifier is extracted only into the installer's disposable scratch directory.
GH_VERIFIER_ASSETS = {
    "aarch64-apple-darwin": (
        "gh_2.101.0_macOS_arm64.zip",
        "e4303e39d8f07141c4bad4b99b01079f05029c59b27076e8fbc825c985ecdd8b",
    ),
    "x86_64-apple-darwin": (
        "gh_2.101.0_macOS_amd64.zip",
        "a6fd66c88e2f07d6e4e058173db341d07dd74d58cf8f19ae668293d2bb614ca3",
    ),
    "x86_64-unknown-linux-gnu": (
        "gh_2.101.0_linux_amd64.tar.gz",
        "9bca2d1c16825f109907a23307628a2f0698fbf99662b73a5cf0b020293072b8",
    ),
}
MAX_GH_BINARY = 100 * 1024 * 1024


class InstallError(Exception):
    pass


class HTTPSOnly(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, new_url):
        if urllib.parse.urlparse(new_url).scheme != "https":
            raise InstallError(f"release redirect is not HTTPS: {new_url}")
        return super().redirect_request(request, fp, code, msg, headers, new_url)


def target_triple() -> str:
    machine = platform.machine().lower()
    system = platform.system()
    if system == "Darwin" and machine in {"arm64", "aarch64"}:
        return "aarch64-apple-darwin"
    if system == "Darwin" and machine == "x86_64":
        return "x86_64-apple-darwin"
    if system == "Linux" and machine == "x86_64":
        libc, libc_version = platform.libc_ver()
        if libc.lower() != "glibc":
            raise InstallError("this bundle requires glibc Linux; musl/unknown libc is unsupported")
        match = re.match(r"^(\d+)\.(\d+)", libc_version)
        if match is None or tuple(map(int, match.groups())) < (2, 35):
            raise InstallError("this Linux bundle requires glibc 2.35 or newer")
        return "x86_64-unknown-linux-gnu"
    raise InstallError(f"unsupported prebuilt platform: {system} {machine}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def expected_sha256(manifest: str, filename: str) -> str:
    matches = []
    for line in manifest.splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64})\s+\*?(.+)", line.strip())
        if match and match.group(2) == filename:
            matches.append(match.group(1).lower())
    if len(matches) != 1:
        raise InstallError(f"expected exactly one SHA256SUMS entry for {filename}")
    return matches[0]


def download(url: str, destination: Path) -> None:
    if urllib.parse.urlparse(url).scheme != "https":
        raise InstallError("release downloads must use HTTPS")
    curl = shutil.which("curl")
    if curl:
        command = [
            curl, "--fail", "--silent", "--show-error", "--location",
            "--proto", "=https", "--proto-redir", "=https",
            "--max-redirs", "8", "--connect-timeout", "30",
            "--max-time", "180", "--max-filesize", str(MAX_ARCHIVE),
            "--output", str(destination), url,
        ]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            raise InstallError(f"HTTPS download failed: {result.stderr.strip() or result.returncode}")
        if destination.stat().st_size > MAX_ARCHIVE:
            raise InstallError("release asset exceeds the installer size limit")
        return
    opener = urllib.request.build_opener(HTTPSOnly())
    request = urllib.request.Request(url, headers={"User-Agent": "rgo-installer/0.1"})
    with opener.open(request, timeout=30) as source, destination.open("xb") as output:
        size = 0
        while block := source.read(1024 * 1024):
            size += len(block)
            if size > MAX_ARCHIVE:
                raise InstallError("release asset exceeds the installer size limit")
            output.write(block)


def verifier_binary(scratch: Path, target: str) -> Path:
    installed = shutil.which("gh")
    if installed:
        try:
            capability = subprocess.run(
                [installed, "attestation", "verify", "--help"],
                capture_output=True,
                timeout=10,
                check=False,
            )
            if capability.returncode == 0 and b"--source-ref" in capability.stdout:
                return Path(installed)
        except (OSError, subprocess.TimeoutExpired):
            pass

    asset, expected = GH_VERIFIER_ASSETS[target]
    verifier_archive = scratch / asset
    download(f"https://github.com/cli/cli/releases/download/v{GH_VERSION}/{asset}", verifier_archive)
    if sha256(verifier_archive) != expected:
        raise InstallError("GitHub CLI verifier SHA-256 mismatch")
    member_name = f"{asset.removesuffix('.zip').removesuffix('.tar.gz')}/bin/gh"
    binary = scratch / "gh"
    if asset.endswith(".zip"):
        with zipfile.ZipFile(verifier_archive) as bundle:
            members = [item for item in bundle.infolist() if item.filename == member_name]
            if len(members) != 1 or members[0].is_dir() or members[0].file_size > MAX_GH_BINARY:
                raise InstallError("GitHub CLI verifier archive has an invalid binary")
            with bundle.open(members[0]) as source:
                copy_verifier(source, binary)
    else:
        with tarfile.open(verifier_archive, "r:gz") as bundle:
            members = [item for item in bundle.getmembers() if item.name == member_name]
            if len(members) != 1 or not members[0].isfile() or members[0].size > MAX_GH_BINARY:
                raise InstallError("GitHub CLI verifier archive has an invalid binary")
            source = bundle.extractfile(members[0])
            if source is None:
                raise InstallError("GitHub CLI verifier archive is truncated")
            with source:
                copy_verifier(source, binary)
    binary.chmod(0o700)
    version = subprocess.run([binary, "--version"], capture_output=True, text=True, check=True)
    if not version.stdout.startswith(f"gh version {GH_VERSION} "):
        raise InstallError("downloaded GitHub CLI verifier has the wrong version")
    return binary


def copy_verifier(source, destination: Path) -> None:
    size = 0
    with destination.open("xb") as output:
        while block := source.read(1024 * 1024):
            size += len(block)
            if size > MAX_GH_BINARY:
                raise InstallError("GitHub CLI verifier binary exceeds the size limit")
            output.write(block)


def verify_attestation(archive: Path, args: argparse.Namespace, scratch: Path, target: str) -> None:
    if args.development_bundle:
        if args.archive is None:
            raise InstallError("--development-bundle is only allowed with a local archive")
        print("development bundle: skipping provenance verification", file=sys.stderr)
        return
    if not args.repo:
        raise InstallError("--repo OWNER/REPO is required to verify release provenance")
    gh = verifier_binary(scratch, target)
    command = [
        str(gh),
        "attestation",
        "verify",
        str(archive),
        "--repo",
        args.repo,
        "--signer-workflow",
        f"{args.repo}/.github/workflows/release.yml",
        "--source-ref",
        f"refs/tags/{args.version}",
    ]
    if bool(args.attestation_bundle) != bool(args.trusted_root):
        raise InstallError("offline verification requires both --attestation-bundle and --trusted-root")
    if args.attestation_bundle:
        command.extend(["--bundle", str(args.attestation_bundle)])
        command.extend(["--custom-trusted-root", str(args.trusted_root)])
    subprocess.run(command, check=True)


def extract_archive(archive: Path, stage: Path, top: str) -> None:
    seen: set[str] = set()
    total = 0
    with tarfile.open(archive, "r:gz") as bundle:
        for member in bundle:
            name = PurePosixPath(member.name)
            if (
                not name.parts
                or name.is_absolute()
                or ".." in name.parts
                or name.parts[0] != top
            ):
                raise InstallError(f"unsafe archive path: {member.name}")
            if member.isdir():
                if len(name.parts) != 1:
                    raise InstallError(f"unexpected archive directory: {member.name}")
                continue
            if not member.isfile() or len(name.parts) != 2 or name.name not in ALLOWED_FILES:
                raise InstallError(f"unexpected archive member: {member.name}")
            if name.name in seen:
                raise InstallError(f"duplicate archive member: {name.name}")
            seen.add(name.name)
            total += member.size
            if total > MAX_UNPACKED:
                raise InstallError("release bundle exceeds the unpacked size limit")
            source = bundle.extractfile(member)
            if source is None:
                raise InstallError(f"cannot read archive member: {member.name}")
            destination = stage / name.name
            with source, destination.open("xb") as output:
                shutil.copyfileobj(source, output)
            if destination.stat().st_size != member.size:
                raise InstallError(f"truncated archive member: {member.name}")
            destination.chmod(0o755 if name.name in {"rgo", "rgo-rustc-wrapper"} else 0o644)
    if not {"rgo", "rgo-rustc-wrapper"}.issubset(seen):
        raise InstallError("release bundle does not contain both executables")


def run(
    program: Path,
    *arguments: str,
    environment: dict[str, str],
    test_abrupt_exit: bool = False,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [str(program), *arguments],
        env=environment,
        text=True,
        capture_output=True,
    )
    if result.returncode:
        if test_abrupt_exit and result.returncode == 88:
            os._exit(88)
        detail = (result.stderr or result.stdout).strip()
        raise InstallError(
            f"{program.name} {' '.join(arguments)} failed ({result.returncode}): "
            f"{detail[:2000]}"
        )
    return result


def checked_directory(path: Path) -> None:
    if path.is_symlink():
        raise InstallError(f"refusing symlinked installation directory {path}")
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    if not path.is_dir():
        raise InstallError(f"installation path is not a directory: {path}")
    details = path.stat()
    if details.st_uid != os.getuid() or details.st_mode & 0o022:
        raise InstallError(f"installation path must be user-owned and not group/world writable: {path}")


def find_real_cargo(search_path: str, shim: Path) -> Path | None:
    for entry in search_path.split(os.pathsep):
        candidate = (Path(entry or ".") / "cargo").absolute()
        if (
            candidate.resolve() != shim.resolve()
            and not is_rgo_cargo_shim(candidate.resolve())
            and candidate.is_file()
            and os.access(candidate, os.X_OK)
        ):
            return candidate
    return None


def is_rgo_cargo_shim(path: Path) -> bool:
    return path.name == "cargo" and path.parent.name == "shims" and path.parent.parent.name == "rgo"


@contextlib.contextmanager
def install_lock(root: Path):
    checked_directory(root)
    lock_path = root / ".install.lock"
    with lock_path.open("a+b") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        yield


def existing_entrypoint(path: Path, versions: Path) -> None:
    if not path.exists() and not path.is_symlink():
        return
    if not path.is_symlink() or not path.resolve().is_relative_to(versions.resolve()):
        raise InstallError(f"{path} is not owned by this installer; refusing to replace it")


def publish_entrypoint(path: Path, target: Path) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.symlink_to(target)
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def restore_entrypoint(path: Path, previous: Path | None) -> None:
    if previous is None:
        path.unlink(missing_ok=True)
    else:
        publish_entrypoint(path, previous)


def write_state(path: Path, value: dict[str, object]) -> None:
    descriptor, name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "w") as output:
            json.dump(value, output, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temporary.unlink(missing_ok=True)


def sync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def activation_paths(
    cargo_home: Path, install_root: Path, record: dict[str, object]
) -> tuple[Path, ...]:
    config = Path(str(record.get("config_file", "")))
    root = Path(str(record.get("rgo_home", "")))
    if config not in {cargo_home / "config", cargo_home / "config.toml"} or not root.is_absolute():
        raise InstallError("activation record names unexpected configuration paths")
    state = root / "state"
    paths = [
        config,
        cargo_home / ".rgo-install.json",
        cargo_home / ".rgo-home",
        state / "inner-wrapper",
        state / "owner-cargo-home",
        state / "storage-mode",
        install_root / "installer-state.json",
    ]
    if record.get("supervised_cargo") is not None:
        shim = record["supervised_cargo"]
        if not isinstance(shim, dict) or shim.get("shim_path") != str(cargo_home / "rgo/shims/cargo"):
            raise InstallError("activation record names an unexpected Cargo shim")
        paths.append(cargo_home / "rgo/shims/cargo")
    return tuple(paths)


def file_snapshot(path: Path) -> tuple[bytes, int] | None:
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    with os.fdopen(descriptor, "rb") as source:
        details = os.fstat(source.fileno())
        if not stat.S_ISREG(details.st_mode):
            raise InstallError(f"activation path is not a regular file: {path}")
        contents = source.read(MAX_ACTIVATION_FILE + 1)
        if len(contents) > MAX_ACTIVATION_FILE:
            raise InstallError(f"activation file exceeds the rollback limit: {path}")
        return contents, stat.S_IMODE(details.st_mode)


def activation_snapshot(paths: tuple[Path, ...]) -> dict[Path, tuple[bytes, int] | None]:
    return {path: file_snapshot(path) for path in paths}


def encode_snapshot(snapshot: dict[Path, tuple[bytes, int] | None]) -> dict[str, object]:
    return {
        str(path): None
        if value is None
        else {"contents": base64.b64encode(value[0]).decode("ascii"), "mode": value[1]}
        for path, value in snapshot.items()
    }


def decode_snapshot(
    encoded: object, expected_paths: tuple[Path, ...]
) -> dict[Path, tuple[bytes, int] | None]:
    if not isinstance(encoded, dict) or set(encoded) != {str(path) for path in expected_paths}:
        raise InstallError("installer journal names unexpected activation files")
    snapshot = {}
    for path in expected_paths:
        value = encoded[str(path)]
        if value is None:
            snapshot[path] = None
            continue
        if not isinstance(value, dict) or not isinstance(value.get("contents"), str):
            raise InstallError(f"invalid journal snapshot for {path}")
        mode = value.get("mode")
        if not isinstance(mode, int) or mode < 0 or mode > 0o7777:
            raise InstallError(f"invalid journal file mode for {path}")
        try:
            contents = base64.b64decode(value["contents"], validate=True)
        except ValueError as error:
            raise InstallError(f"invalid journal data for {path}") from error
        if len(contents) > MAX_ACTIVATION_FILE:
            raise InstallError(f"oversized journal snapshot for {path}")
        snapshot[path] = (contents, mode)
    return snapshot


def state_bytes(value: dict[str, object]) -> bytes:
    return (json.dumps(value, sort_keys=True) + "\n").encode()


def planned_snapshot(
    encoded: object,
    before: dict[Path, tuple[bytes, int] | None],
    state_path: Path,
    new_state: dict[str, object],
) -> dict[Path, tuple[bytes, int] | None]:
    if not isinstance(encoded, dict) or encoded.get("schema_version") != 1:
        raise InstallError("replacement setup returned an invalid activation plan")
    files = encoded.get("files")
    if not isinstance(files, dict) or not set(files).issubset({str(path) for path in before}):
        raise InstallError("replacement setup planned unexpected activation paths")
    planned = before.copy()
    for name, value in files.items():
        path = Path(name)
        if value is None:
            planned[path] = None
            continue
        if not isinstance(value, dict) or not isinstance(value.get("contents"), str):
            raise InstallError(f"invalid planned activation file {name}")
        mode = value.get("mode")
        if not isinstance(mode, int) or mode < 0 or mode > 0o7777:
            raise InstallError(f"invalid planned activation mode {name}")
        contents = value["contents"].encode()
        if len(contents) > MAX_ACTIVATION_FILE:
            raise InstallError(f"planned activation file exceeds rollback limit: {name}")
        planned[path] = (contents, mode)
    planned[state_path] = (state_bytes(new_state), 0o600)
    if str(state_path) in files:
        raise InstallError("setup plan unexpectedly modifies installer state")
    return planned


def restore_file(path: Path, previous: tuple[bytes, int] | None) -> None:
    if previous is None:
        path.unlink(missing_ok=True)
        return
    contents, mode = previous
    descriptor, name = tempfile.mkstemp(prefix=f".{path.name}.rollback-", dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(contents)
            output.flush()
            os.fsync(output.fileno())
        temporary.chmod(mode)
        os.replace(temporary, path)
        sync_directory(path.parent)
    finally:
        temporary.unlink(missing_ok=True)


def shell_profiles(home: Path, shell: str) -> tuple[Path, ...]:
    name = Path(shell).name
    if name == "zsh":
        if os.environ.get("ZDOTDIR") and Path(os.environ["ZDOTDIR"]).expanduser().absolute() != home:
            return ()
        return home / ".zprofile", home / ".zshrc"
    if name == "bash":
        login = next(
            (home / name for name in (".bash_profile", ".bash_login", ".profile") if (home / name).exists() or (home / name).is_symlink()),
            home / ".bash_profile",
        )
        return login, home / ".bashrc"
    return ()


def shell_block(cargo_home: Path, *, legacy: bool = False) -> bytes:
    shim = str(cargo_home / "rgo" / "shims")
    if any(character in shim for character in (":", "\n", "\r")):
        raise InstallError("supervised Cargo PATH cannot contain a colon or newline")
    identity = hashlib.sha256(os.fsencode(cargo_home)).hexdigest()[:16]
    path_edit = (
        'case ":$PATH:" in *":$rgo_shim_dir:"*) ;; *) export PATH="$rgo_shim_dir:$PATH" ;; esac\n'
        if legacy
        else 'case "$PATH" in "$rgo_shim_dir"|"$rgo_shim_dir":*) ;; *) export PATH="$rgo_shim_dir:$PATH" ;; esac\n'
    )
    return (
        f"\n# >>> rgo supervised Cargo {identity} >>>\n"
        f"rgo_shim_dir={shlex.quote(shim)}\n"
        f"{path_edit}"
        "unset rgo_shim_dir\n"
        f"# <<< rgo supervised Cargo {identity} <<<\n"
    ).encode("utf-8", "surrogateescape")


def checked_profile(path: Path) -> tuple[bytes, int] | None:
    snapshot = file_snapshot(path)
    if snapshot is not None and path.stat(follow_symlinks=False).st_uid != os.getuid():
        raise InstallError(f"shell profile is not user-owned: {path}")
    return snapshot


def shell_activation_state(install_root: Path, cargo_home: Path, home: Path) -> dict[str, object] | None:
    state_path = install_root / "shell-activation.json"
    snapshot = checked_profile(state_path)
    if snapshot is None:
        return None
    state = json.loads(snapshot[0])
    if not isinstance(state, dict) or state.get("schema_version") != 1 or state.get("cargo_home") != str(cargo_home) or state.get("home") != str(home):
        raise InstallError("shell activation state belongs to a different installation or home")
    profiles = state.get("profiles")
    allowed = {str(home / name) for name in (".zprofile", ".zshrc", ".bash_profile", ".bash_login", ".profile", ".bashrc")}
    if not isinstance(profiles, list) or not profiles or len(profiles) != len(set(str(item.get("path")) for item in profiles if isinstance(item, dict))):
        raise InstallError("invalid shell activation profile list")
    if any(not isinstance(item, dict) or item.get("path") not in allowed or not isinstance(item.get("existed"), bool) for item in profiles):
        raise InstallError("shell activation state names an unexpected profile")
    return state


def check_shell_block(path: Path, block: bytes, legacy_block: bytes) -> tuple[bytes, int] | None:
    snapshot = checked_profile(path)
    if snapshot is not None:
        contents = snapshot[0]
        marker = block.splitlines()[1]
        known_block = block in contents or legacy_block in contents
        if contents.count(marker) > 1 or (marker in contents and not known_block):
            raise InstallError(f"owned shell activation changed in {path}; refusing to overwrite it")
        if not known_block and b"# >>> rgo supervised Cargo " in contents:
            # Another installation can coexist, but this installation must not
            # adopt an untracked block with the same identity.
            identity = marker.split()[5]
            if identity in contents:
                raise InstallError(f"untracked shell activation exists in {path}")
    return snapshot


def ensure_shell_activation(
    install_root: Path, cargo_home: Path, home: Path, shell: str,
    development_probe: bool = False,
) -> bool:
    profiles = shell_profiles(home, shell)
    state = shell_activation_state(install_root, cargo_home, home)
    block = shell_block(cargo_home)
    legacy_block = shell_block(cargo_home, legacy=True)
    if state is None:
        if not profiles:
            return False
        if home.is_symlink() or not home.is_dir() or home.stat().st_uid != os.getuid() or home.stat().st_mode & 0o022:
            raise InstallError(f"shell home must be user-owned and not group/world writable: {home}")
        entries = []
        for path in profiles:
            snapshot = check_shell_block(path, block, legacy_block)
            if snapshot is not None and (block in snapshot[0] or legacy_block in snapshot[0]):
                raise InstallError(f"untracked shell activation exists in {path}")
            entries.append({"path": str(path), "existed": snapshot is not None})
        state = {"schema_version": 1, "cargo_home": str(cargo_home), "home": str(home), "profiles": entries}
        write_state(install_root / "shell-activation.json", state)
    for index, entry in enumerate(state["profiles"]):
        path = Path(entry["path"])
        snapshot = check_shell_block(path, block, legacy_block)
        if snapshot is not None and block in snapshot[0]:
            continue
        contents, mode = snapshot if snapshot is not None else (b"", 0o600)
        updated = (
            contents.replace(legacy_block, block, 1)
            if legacy_block in contents
            else contents + block
        )
        if len(updated) > MAX_ACTIVATION_FILE:
            raise InstallError(f"shell profile exceeds the activation size limit: {path}")
        if checked_profile(path) != snapshot:
            raise InstallError(f"shell profile changed during activation: {path}")
        restore_file(path, (updated, mode))
        if index == 0 and development_probe and os.environ.get("RGO_INSTALLER_TEST_EXIT_AFTER_FIRST_PROFILE") == "1":
            os._exit(92)
    return True


def remove_shell_activation(install_root: Path, cargo_home: Path, home: Path) -> None:
    state = shell_activation_state(install_root, cargo_home, home)
    if state is None:
        return
    block = shell_block(cargo_home)
    legacy_block = shell_block(cargo_home, legacy=True)
    # Validate all profiles before changing any of them.
    snapshots = {
        Path(entry["path"]): check_shell_block(Path(entry["path"]), block, legacy_block)
        for entry in state["profiles"]
    }
    for entry in state["profiles"]:
        path = Path(entry["path"])
        snapshot = snapshots[path]
        if snapshot is None:
            continue
        owned_block = block if block in snapshot[0] else legacy_block
        if owned_block not in snapshot[0]:
            continue
        if checked_profile(path) != snapshot:
            raise InstallError(f"shell profile changed during uninstall: {path}")
        head, tail = snapshot[0].split(owned_block, 1)
        # The owned block starts with a separator newline. If a user added
        # content after it, preserve the separation from an original file
        # whose final line had no newline.
        separator = b"\n" if head and tail and not head.endswith(b"\n") else b""
        contents = head + separator + tail
        if not contents and not entry["existed"]:
            path.unlink()
            sync_directory(path.parent)
        else:
            restore_file(path, (contents, snapshot[1]))
    (install_root / "shell-activation.json").unlink()
    sync_directory(install_root)


def restore_activation(
    cargo_home: Path,
    root: Path,
    before: dict[Path, tuple[bytes, int] | None],
    after: dict[Path, tuple[bytes, int] | None],
) -> None:
    # Match setup's lock order. A user edit made after the replacement setup
    # must never be overwritten by a delayed installer rollback.
    setup_lock = cargo_home / ".rgo-setup.lock"
    root_lock = root / "state/.rgo-service.lock"
    if setup_lock.is_symlink() or root_lock.is_symlink():
        raise InstallError("refusing symlinked setup lock during rollback")
    with setup_lock.open("a+b") as home_lock, root_lock.open("a+b") as storage_lock:
        fcntl.flock(home_lock.fileno(), fcntl.LOCK_EX)
        fcntl.flock(storage_lock.fileno(), fcntl.LOCK_EX)
        current = activation_snapshot(tuple(before))
        for path, value in current.items():
            if value != before[path] and value != after[path]:
                raise InstallError(f"activation changed during upgrade; refusing to overwrite {path}")
        for path, value in current.items():
            if value != before[path]:
                restore_file(path, before[path])


def link_snapshot(paths: tuple[Path, ...]) -> dict[Path, Path | None]:
    result = {}
    for path in paths:
        if path.is_symlink():
            result[path] = Path(os.readlink(path))
        elif path.exists():
            raise InstallError(f"command entrypoint is not an owned link: {path}")
        else:
            result[path] = None
    return result


def encode_links(links: dict[Path, Path | None]) -> dict[str, str | None]:
    return {str(path): None if target is None else str(target) for path, target in links.items()}


def decode_links(encoded: object, paths: tuple[Path, ...], versions: Path) -> dict[Path, Path | None]:
    if not isinstance(encoded, dict) or set(encoded) != {str(path) for path in paths}:
        raise InstallError("installer journal names unexpected command links")
    links = {}
    for path in paths:
        value = encoded[str(path)]
        if value is None:
            links[path] = None
            continue
        if not isinstance(value, str):
            raise InstallError(f"invalid journal command link {path}")
        target = Path(value)
        resolved = (target if target.is_absolute() else path.parent / target).resolve()
        if not resolved.is_relative_to(versions.resolve()):
            raise InstallError(f"journal command link escapes owned versions: {path}")
        links[path] = target
    return links


def check_links_within(
    paths: tuple[Path, ...], before: dict[Path, Path | None], after: dict[Path, Path | None]
) -> dict[Path, Path | None]:
    current = link_snapshot(paths)
    for path, value in current.items():
        if value != before[path] and value != after[path]:
            raise InstallError(f"command link changed during upgrade: {path}")
    return current


def read_journal(path: Path) -> dict[str, object] | None:
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return None
    with os.fdopen(descriptor, "rb") as source:
        details = os.fstat(source.fileno())
        if not stat.S_ISREG(details.st_mode) or details.st_size > MAX_JOURNAL:
            raise InstallError(f"invalid or oversized installer journal {path}")
        contents = source.read(MAX_JOURNAL + 1)
    if len(contents) > MAX_JOURNAL:
        raise InstallError(f"oversized installer journal {path}")
    value = json.loads(contents)
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise InstallError(f"unsupported installer journal {path}")
    return value


def recover_journal(
    journal_path: Path, cargo_home: Path, install_root: Path, bin_dir: Path, versions: Path
) -> None:
    journal = read_journal(journal_path)
    if journal is None:
        return
    if journal.get("cargo_home") != str(cargo_home) or journal.get("install_root") != str(install_root):
        raise InstallError("installer journal belongs to a different Cargo home or install root")
    record_path = cargo_home / ".rgo-install.json"
    encoded_before = journal.get("before")
    if not isinstance(encoded_before, dict):
        raise InstallError("installer journal has no previous activation snapshot")
    first_install = journal.get("first_install") is True
    if first_install:
        if encoded_before.get(str(record_path)) is not None:
            raise InstallError("first-install journal unexpectedly contains a prior activation")
        old_record = {
            "config_file": journal.get("config_file"),
            "rgo_home": journal.get("rgo_home"),
            "supervised_cargo": {} if journal.get("supervised") is True else None,
        }
        if journal.get("supervised") is True:
            old_record["supervised_cargo"] = {"shim_path": str(cargo_home / "rgo/shims/cargo")}
    else:
        if not isinstance(encoded_before.get(str(record_path)), dict):
            raise InstallError("installer journal has no previous activation record")
        record_data = encoded_before[str(record_path)].get("contents")
        if not isinstance(record_data, str):
            raise InstallError("installer journal has an invalid previous record")
        old_record = json.loads(base64.b64decode(record_data, validate=True))
        if not isinstance(old_record, dict):
            raise InstallError("installer journal previous record is malformed")
    tracked = activation_paths(cargo_home, install_root, old_record)
    before = decode_snapshot(encoded_before, tracked)
    root = Path(str(old_record.get("rgo_home", "")))
    if journal.get("rgo_home") != str(root):
        raise InstallError("installer journal storage root does not match the previous record")
    links = (bin_dir / "rgo-rustc-wrapper", bin_dir / "rgo")
    old_links = decode_links(journal.get("old_links"), links, versions)
    new_links = decode_links(journal.get("new_links"), links, versions)
    if first_install:
        if any(target is not None for target in old_links.values()):
            raise InstallError("first-install journal unexpectedly contains prior command links")
    else:
        previous_cli = Path(str(old_record.get("rgo_binary", ""))).resolve()
        old_wrapper = previous_cli.with_name("rgo-rustc-wrapper")
        if (
            previous_cli.name != "rgo"
            or old_record.get("install_root") != str(previous_cli.parent)
            or not isinstance(old_record.get("binary_version"), str)
            or not previous_cli.is_file()
            or not previous_cli.is_relative_to(versions.resolve())
            or not old_wrapper.is_file()
            or old_links[bin_dir / "rgo"] is None
            or old_links[bin_dir / "rgo-rustc-wrapper"] is None
            or (bin_dir / old_links[bin_dir / "rgo"]).resolve() != previous_cli
            or (bin_dir / old_links[bin_dir / "rgo-rustc-wrapper"]).resolve() != old_wrapper
        ):
            raise InstallError("the previous versioned binary is unavailable for journal recovery")
    phase = journal.get("phase")
    if phase == "prepared":
        current_links = link_snapshot(links)
        if current_links != old_links:
            raise InstallError("command links changed before setup completed; refusing recovery")
        if activation_snapshot(tracked) != before:
            if journal.get("after") is None:
                raise InstallError("setup stopped before a replacement snapshot was recorded; repair the partial activation")
            planned = decode_snapshot(journal["after"], tracked)
            restore_activation(cargo_home, root, before, planned)
        journal_path.unlink()
        print("recovered interrupted installer setup")
        return
    if phase not in {"setup_done", "committed"}:
        raise InstallError("installer journal has an unknown phase")
    after = decode_snapshot(journal.get("after"), tracked)
    current_links = check_links_within(links, old_links, new_links)
    if phase == "committed":
        if activation_snapshot(tracked) != after or current_links != new_links:
            raise InstallError("committed upgrade state changed; refusing automatic journal removal")
        journal_path.unlink()
        print("confirmed previously committed installer transaction")
        return
    restore_activation(cargo_home, root, before, after)
    for path, previous in old_links.items():
        if current_links[path] != previous:
            restore_entrypoint(path, previous)
    journal_path.unlink()
    print("restored prior activation after interrupted installer transaction")


def repair_owned(
    stage: Path,
    version_dir: Path,
    cargo_home: Path,
    install_root: Path,
    bin_dir: Path,
    environment: dict[str, str],
    version: str,
    supervised: bool,
    test_exit_after_first_replacement: bool = False,
) -> None:
    if version_dir.is_symlink() or not version_dir.is_dir():
        raise InstallError("repair requires an existing owned version directory")
    details = version_dir.stat()
    if details.st_uid != os.getuid() or details.st_mode & 0o022:
        raise InstallError("repair requires a private user-owned version directory")
    state_path = install_root / "installer-state.json"
    record_path = cargo_home / ".rgo-install.json"
    state_file = file_snapshot(state_path)
    record_file = file_snapshot(record_path)
    if state_file is None or record_file is None:
        raise InstallError("repair requires the installer state and Cargo activation record")
    state = json.loads(state_file[0])
    record = json.loads(record_file[0])
    cli = version_dir / "rgo"
    wrapper = version_dir / "rgo-rustc-wrapper"
    if (
        not isinstance(state, dict)
        or not isinstance(record, dict)
        or state.get("schema_version") != 1
        or state.get("no_service") is not True
        or state.get("supervised", False) is not supervised
        or Path(str(state.get("rgo_binary", ""))).resolve() != cli.resolve()
        or Path(str(record.get("rgo_binary", ""))).resolve() != cli.resolve()
        or Path(str(record.get("install_root", ""))).resolve() != version_dir.resolve()
        or record.get("cargo_home") != str(cargo_home)
        or record.get("binary_version") != version.removeprefix("v")
        or (
            record.get("wrapper_binary") is not None
            and Path(str(record["wrapper_binary"])).resolve() != wrapper.resolve()
        )
    ):
        raise InstallError("repair bundle does not match the owned no-service activation")
    expected_version = version.removeprefix("v")
    expected_wrapper_banner = (
        f"rgo-rustc-wrapper {expected_version} protocol {record.get('protocol_version')}"
    )
    if run(stage / "rgo-rustc-wrapper", "--rgo-version", environment=environment).stdout.strip() != expected_wrapper_banner:
        raise InstallError("verified repair bundle has a different wrapper protocol")
    activation_paths(cargo_home, install_root, record)
    root = Path(str(record["rgo_home"]))
    if environment.get("RGO_HOME") and Path(environment["RGO_HOME"]).absolute() != root:
        raise InstallError("repair RGO_HOME differs from the activation record")
    environment["RGO_HOME"] = str(root)
    for name, target in (("rgo", cli), ("rgo-rustc-wrapper", wrapper)):
        link = bin_dir / name
        if not link.is_symlink() or link.resolve() != target.resolve():
            raise InstallError(f"repair command link is not owned by this version: {link}")
    expected_names = {member.name for member in stage.iterdir()}
    existing_names = {member.name for member in version_dir.iterdir()}
    if not existing_names.issubset(expected_names):
        raise InstallError("repair version directory contains unexpected files")
    for name in expected_names:
        existing = version_dir / name
        if existing.is_symlink() or (existing.exists() and not existing.is_file()):
            raise InstallError(f"repair refuses unexpected version member {existing}")
    replaced = []
    for name in sorted(expected_names):
        existing = version_dir / name
        candidate = stage / name
        if not existing.exists() or sha256(existing) != sha256(candidate):
            os.replace(candidate, existing)
            replaced.append(name)
            if test_exit_after_first_replacement:
                os._exit(89)
    directory = os.open(version_dir, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
    if run(cli, "--version", environment=environment).stdout.strip() != f"rgo {expected_version}":
        raise InstallError("repaired rgo binary has an unexpected version")
    wrapper_banner = run(wrapper, "--rgo-version", environment=environment).stdout.strip()
    if wrapper_banner != expected_wrapper_banner:
        raise InstallError("repaired wrapper does not match the activation protocol")
    verify_environment = environment.copy()
    verify_environment.pop("RGO_HOME", None)
    if supervised:
        shim_dir = cargo_home / "rgo/shims"
        verify_environment["PATH"] = f"{shim_dir}{os.pathsep}{environment.get('PATH', '')}"
    doctor = run(cli, "doctor", "--verify", "--json", environment=verify_environment)
    if json.loads(doctor.stdout).get("activation_verified") is not True:
        raise InstallError("repaired binaries did not restore plain Cargo activation")
    print(f"repaired {', '.join(replaced) if replaced else 'verified files'} in {version_dir}")


def uninstall_owned(
    cargo_home: Path,
    install_root: Path,
    bin_dir: Path,
    environment: dict[str, str],
    development_probe: bool,
) -> None:
    versions = install_root / "versions"
    if not install_root.exists() and not install_root.is_symlink():
        record = cargo_home / ".rgo-install.json"
        commands = (bin_dir / "rgo", bin_dir / "rgo-rustc-wrapper")
        if not record.exists() and not record.is_symlink() and all(
            not path.exists() and not path.is_symlink() for path in commands
        ):
            print("rgo installer activation is already removed")
            return
        raise InstallError("installer root is missing while Cargo activation or command links remain")
    with install_lock(install_root):
        checked_directory(versions)
        checked_directory(bin_dir)
        links = (bin_dir / "rgo-rustc-wrapper", bin_dir / "rgo")
        for link in links:
            existing_entrypoint(link, versions)
        recover_journal(
            install_root / "installer-transaction.json",
            cargo_home, install_root, bin_dir, versions,
        )
        journal_path = install_root / "installer-uninstall.json"
        journal = read_journal(journal_path)
        record_path = cargo_home / ".rgo-install.json"
        state_path = install_root / "installer-state.json"
        if journal is None:
            record_file = file_snapshot(record_path)
            state_file = file_snapshot(state_path)
            if record_file is None and state_file is None and all(
                target is None for target in link_snapshot(links).values()
            ):
                print("rgo installer activation is already removed")
                return
            if record_file is None or state_file is None:
                raise InstallError("uninstall requires an owned activation record and installer state")
            record = json.loads(record_file[0])
            state = json.loads(state_file[0])
            if not isinstance(record, dict) or not isinstance(state, dict):
                raise InstallError("invalid owned installer state")
            cli = Path(str(record.get("rgo_binary", ""))).resolve()
            wrapper = cli.with_name("rgo-rustc-wrapper")
            if (
                cli.name != "rgo"
                or not cli.is_file()
                or not cli.is_relative_to(versions.resolve())
                or Path(str(record.get("install_root", ""))).resolve() != cli.parent
                or record.get("cargo_home") != str(cargo_home)
                or state.get("schema_version") != 1
                or state.get("no_service") is not True
                or state.get("supervised", False) is not (record.get("supervised_cargo") is not None)
                or Path(str(state.get("rgo_binary", ""))).resolve() != cli
            ):
                raise InstallError("uninstall requires a matching owned no-service installation")
            activation_paths(cargo_home, install_root, record)
            previous_links = link_snapshot(links)
            if (
                previous_links[bin_dir / "rgo"] is None
                or previous_links[bin_dir / "rgo-rustc-wrapper"] is None
                or (bin_dir / previous_links[bin_dir / "rgo"]).resolve() != cli
                or (bin_dir / previous_links[bin_dir / "rgo-rustc-wrapper"]).resolve() != wrapper
            ):
                raise InstallError("owned command links changed before uninstall")
            if run(cli, "--version", environment=environment).stdout.strip() != f"rgo {record.get('binary_version')}":
                raise InstallError("owned rgo binary does not match its activation record")
            journal = {
                "schema_version": 1,
                "phase": "prepared",
                "cargo_home": str(cargo_home),
                "install_root": str(install_root),
                "bin_dir": str(bin_dir),
                "before": encode_snapshot({record_path: record_file, state_path: state_file}),
                "links": encode_links(previous_links),
            }
            write_state(journal_path, journal)
        elif (
            journal.get("cargo_home") != str(cargo_home)
            or journal.get("install_root") != str(install_root)
            or journal.get("bin_dir") != str(bin_dir)
        ):
            raise InstallError("uninstall journal belongs to another installation")

        before = decode_snapshot(journal.get("before"), (record_path, state_path))
        record_file = before[record_path]
        state_file = before[state_path]
        if record_file is None or state_file is None:
            raise InstallError("uninstall journal lacks its owned activation")
        record = json.loads(record_file[0])
        state = json.loads(state_file[0])
        if not isinstance(record, dict) or not isinstance(state, dict):
            raise InstallError("uninstall journal has an invalid activation")
        if journal.get("phase") not in {"prepared", "setup_done", "committed"}:
            raise InstallError("uninstall journal has an unknown phase")
        cli = Path(str(record.get("rgo_binary", ""))).resolve()
        root = Path(str(record.get("rgo_home", "")))
        tracked = activation_paths(cargo_home, install_root, record)
        if (
            cli.name != "rgo"
            or not cli.is_file()
            or not cli.is_relative_to(versions.resolve())
            or Path(str(record.get("install_root", ""))).resolve() != cli.parent
            or record.get("cargo_home") != str(cargo_home)
            or state.get("schema_version") != 1
            or state.get("no_service") is not True
            or state.get("supervised", False) is not (record.get("supervised_cargo") is not None)
            or Path(str(state.get("rgo_binary", ""))).resolve() != cli
        ):
            raise InstallError("uninstall journal no longer names an owned binary")
        if run(cli, "--version", environment=environment).stdout.strip() != f"rgo {record.get('binary_version')}":
            raise InstallError("owned rgo binary changed during uninstall")
        old_links = decode_links(journal.get("links"), links, versions)
        if (
            old_links[bin_dir / "rgo"] is None
            or old_links[bin_dir / "rgo-rustc-wrapper"] is None
            or (bin_dir / old_links[bin_dir / "rgo"]).resolve() != cli
            or (bin_dir / old_links[bin_dir / "rgo-rustc-wrapper"]).resolve()
            != cli.with_name("rgo-rustc-wrapper")
        ):
            raise InstallError("uninstall journal has unexpected command links")
        current_record = file_snapshot(record_path)
        if current_record is not None and current_record != record_file:
            raise InstallError("activation record changed during uninstall")
        current_state = file_snapshot(state_path)
        if current_state is not None and current_state != state_file:
            raise InstallError("installer state changed during uninstall")
        current_links = link_snapshot(links)
        if current_state is None and any(target is not None for target in current_links.values()):
            raise InstallError("installer state disappeared before command links were removed")
        for path, target in current_links.items():
            if target is not None and target != old_links[path]:
                raise InstallError(f"command link changed during uninstall: {path}")

        if environment.get("RGO_HOME") and Path(environment["RGO_HOME"]).expanduser().absolute() != root:
            raise InstallError("uninstall RGO_HOME differs from the activation record")
        environment["RGO_HOME"] = str(root)
        remove_shell_activation(
            install_root, cargo_home,
            Path(environment.get("HOME") or Path.home()).expanduser().absolute(),
        )
        if current_record is not None or journal.get("phase") == "prepared":
            run(cli, "setup", "--undo", "--no-service", environment=environment)
            journal["phase"] = "setup_done"
            write_state(journal_path, journal)
            if development_probe and os.environ.get("RGO_INSTALLER_TEST_EXIT_AFTER_UNDO") == "1":
                os._exit(90)
        if journal.get("phase") not in {"setup_done", "committed"}:
            raise InstallError("uninstall journal has an unknown phase")
        setup_lock = cargo_home / ".rgo-setup.lock"
        root_lock = root / "state/.rgo-service.lock"
        if setup_lock.is_symlink() or root_lock.is_symlink():
            raise InstallError("refusing symlinked setup lock during uninstall")
        with setup_lock.open("a+b") as home_lock, root_lock.open("a+b") as storage_lock:
            fcntl.flock(home_lock.fileno(), fcntl.LOCK_EX)
            fcntl.flock(storage_lock.fileno(), fcntl.LOCK_EX)
            config = Path(str(record["config_file"]))
            config_file = file_snapshot(config)
            if (
                file_snapshot(record_path) is not None
                or file_snapshot(cargo_home / ".rgo-home") is not None
                or file_snapshot(root / "state/inner-wrapper") is not None
                or file_snapshot(root / "state/owner-cargo-home") is not None
                or (config_file is not None and b"# >>> rgo managed" in config_file[0])
            ):
                raise InstallError("Cargo activation still exists after setup undo; command links retained")
            if record.get("supervised_cargo") is not None and file_snapshot(
                cargo_home / "rgo/shims/cargo"
            ) is not None:
                raise InstallError("supervised Cargo launcher remains after setup undo")
            current_state = file_snapshot(state_path)
            if current_state is not None and current_state != state_file:
                raise InstallError("installer state changed during uninstall")
            for path, target in link_snapshot(links).items():
                if target is not None and target != old_links[path]:
                    raise InstallError(f"command link changed during uninstall: {path}")
            for index, path in enumerate(links):
                if path.is_symlink():
                    path.unlink()
                    if index == 0 and development_probe and os.environ.get("RGO_INSTALLER_TEST_EXIT_AFTER_FIRST_UNLINK") == "1":
                        os._exit(91)
            if current_state is not None:
                state_path.unlink()
        journal["phase"] = "committed"
        write_state(journal_path, journal)
        journal_path.unlink()
        print("removed owned rgo command links and Cargo activation")
        print(f"retained versioned binaries at {cli.parent} for any Cargo process already using them")
        print(f"retained managed data at {root}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, help="downloaded release .tar.gz")
    parser.add_argument("--repo", help="GitHub OWNER/REPO for download and provenance verification")
    parser.add_argument("--version", help="exact release tag, such as v0.1.0")
    parser.add_argument("--sha256sums", type=Path, help="local SHA256SUMS.txt")
    parser.add_argument("--sha256", help="expected archive digest for a local bundle")
    parser.add_argument("--attestation-bundle", type=Path)
    parser.add_argument("--trusted-root", type=Path)
    parser.add_argument("--development-bundle", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--cargo-home", type=Path)
    parser.add_argument("--rgo-home", type=Path)
    parser.add_argument("--install-root", type=Path)
    parser.add_argument("--bin-dir", type=Path)
    parser.add_argument("--no-service", action="store_true")
    parser.add_argument("--supervised", action="store_true", help="install the opt-in Cargo PATH launcher")
    parser.add_argument("--real-cargo", type=Path, help="absolute path to the existing Cargo proxy")
    parser.add_argument("--verify-only", action="store_true")
    parser.add_argument("--repair", action="store_true", help="restore an owned no-service version from this verified bundle")
    parser.add_argument("--uninstall", action="store_true", help="undo an owned no-service install and remove its command links")
    args = parser.parse_args()
    if args.uninstall and (
        args.repair or args.verify_only or args.archive or args.repo or args.version
        or args.sha256 or args.sha256sums or args.attestation_bundle or args.trusted_root
        or args.supervised or args.real_cargo
    ):
        raise InstallError("--uninstall cannot be combined with release, repair, or verification inputs")
    if args.repair and (args.verify_only or not args.no_service):
        raise InstallError("--repair requires --no-service and cannot be combined with --verify-only")
    if args.real_cargo and not args.supervised:
        raise InstallError("--real-cargo requires --supervised")
    if args.supervised and not args.no_service and not args.verify_only:
        raise InstallError("supervised installer activation currently requires --no-service until service-managed uninstall is supported")

    if os.name != "posix" or platform.system() not in {"Darwin", "Linux"}:
        raise InstallError("this installer supports macOS and glibc Linux; Windows uses a separate installer")
    if not args.uninstall and (not args.version or not VERSION.fullmatch(args.version)):
        raise InstallError("--version must be an exact vMAJOR.MINOR.PATCH tag")
    if args.repo and (
        not REPO.fullmatch(args.repo)
        or any(part in {".", ".."} for part in args.repo.split("/"))
    ):
        raise InstallError("--repo must be OWNER/REPO")
    if not args.uninstall and not args.repo and not args.development_bundle:
        raise InstallError("--repo OWNER/REPO is required to download or verify a release")
    if args.development_bundle and not args.archive and not args.uninstall:
        raise InstallError("--development-bundle is only allowed with a local archive")
    if args.sha256 and not re.fullmatch(r"[0-9a-fA-F]{64}", args.sha256):
        raise InstallError("--sha256 must contain 64 hexadecimal characters")
    cargo_home = args.cargo_home or Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    cargo_home = cargo_home.expanduser().absolute()
    install_root = (args.install_root or cargo_home / "rgo").expanduser().absolute()
    bin_dir = (args.bin_dir or cargo_home / "bin").expanduser().absolute()
    versions = install_root / "versions"
    environment = os.environ.copy()
    environment["CARGO_HOME"] = str(cargo_home)
    if args.rgo_home:
        environment["RGO_HOME"] = str(args.rgo_home.expanduser().absolute())
    if args.uninstall:
        uninstall_owned(cargo_home, install_root, bin_dir, environment, args.development_bundle)
        return
    target = target_triple()
    top = f"rgo-{args.version}-{target}"
    asset = f"{top}.tar.gz"
    version_dir = versions / top
    real_cargo = None
    if args.supervised:
        found = args.real_cargo or find_real_cargo(
            environment.get("PATH", ""), cargo_home / "rgo" / "shims" / "cargo"
        )
        if not found:
            raise InstallError("Cargo was not found on PATH; pass --real-cargo /absolute/path/to/cargo")
        real_cargo = Path(found).expanduser().absolute()
        if (
            real_cargo.name != "cargo"
            or not real_cargo.is_file()
            or not os.access(real_cargo, os.X_OK)
            or real_cargo.resolve() == (cargo_home / "rgo/shims/cargo").resolve()
            or is_rgo_cargo_shim(real_cargo.resolve())
        ):
            raise InstallError("--real-cargo must identify an existing executable named cargo")

    with tempfile.TemporaryDirectory(prefix="rgo-download-") as temporary:
        scratch = Path(temporary)
        archive = scratch / asset
        if args.archive:
            if args.archive.stat().st_size > MAX_ARCHIVE:
                raise InstallError("release asset exceeds the installer size limit")
            shutil.copyfile(args.archive, archive)
        else:
            base = f"https://github.com/{args.repo}/releases/download/{args.version}"
            download(f"{base}/{asset}", archive)
            download(f"{base}/SHA256SUMS.txt", scratch / "SHA256SUMS.txt")
        if args.sha256sums and args.sha256:
            raise InstallError("use either --sha256sums or --sha256")
        if args.sha256sums:
            expected = expected_sha256(args.sha256sums.read_text(), asset)
        elif args.sha256:
            expected = args.sha256.lower()
        elif not args.archive:
            expected = expected_sha256((scratch / "SHA256SUMS.txt").read_text(), asset)
        else:
            raise InstallError("a local archive requires --sha256sums or --sha256")
        actual = sha256(archive)
        if actual != expected:
            raise InstallError(f"archive SHA-256 mismatch: expected {expected}, got {actual}")
        verify_attestation(archive, args, scratch, target)

        with tempfile.TemporaryDirectory(prefix="rgo-verify-") as verification:
            extracted = Path(verification)
            extract_archive(archive, extracted, top)
            cli = extracted / "rgo"
            wrapper = extracted / "rgo-rustc-wrapper"
            cli_version = run(cli, "--version", environment=environment).stdout.strip()
            wrapper_version = run(wrapper, "--rgo-version", environment=environment).stdout.strip()
            expected_version = args.version[1:]
            if cli_version != f"rgo {expected_version}" or not wrapper_version.startswith(
                f"rgo-rustc-wrapper {expected_version} protocol "
            ):
                raise InstallError("archive contains mismatched or incorrectly versioned executables")
            if args.verify_only:
                print(f"verified {asset} ({actual})")
                return

        with install_lock(install_root):
            checked_directory(versions)
            checked_directory(bin_dir)
            for name in ("rgo", "rgo-rustc-wrapper"):
                existing_entrypoint(bin_dir / name, versions)
            journal_path = install_root / "installer-transaction.json"
            recover_journal(journal_path, cargo_home, install_root, bin_dir, versions)
            uninstall_journal = install_root / "installer-uninstall.json"
            if uninstall_journal.exists() or uninstall_journal.is_symlink():
                raise InstallError("an uninstall is pending; finish it with --uninstall before installing")

            stage = Path(tempfile.mkdtemp(prefix=".stage-", dir=versions))
            try:
                extract_archive(archive, stage, top)
                if args.repair:
                    repair_owned(
                        stage, version_dir, cargo_home, install_root, bin_dir,
                        environment, args.version, args.supervised,
                        args.development_bundle
                        and os.environ.get("RGO_INSTALLER_TEST_EXIT_DURING_REPAIR") == "1",
                    )
                    if args.supervised:
                        ensure_shell_activation(
                            install_root, cargo_home,
                            Path(environment.get("HOME") or Path.home()).expanduser().absolute(),
                            environment.get("SHELL", ""),
                        )
                    return
                if version_dir.exists() or version_dir.is_symlink():
                    if version_dir.is_symlink() or not version_dir.is_dir():
                        raise InstallError(f"invalid existing version directory {version_dir}")
                    if {file.name for file in version_dir.iterdir()} != {
                        file.name for file in stage.iterdir()
                    }:
                        raise InstallError(f"existing {version_dir} has unexpected contents")
                    for name in ALLOWED_FILES:
                        candidate = stage / name
                        existing = version_dir / name
                        if existing.is_symlink() or (existing.exists() and not existing.is_file()):
                            raise InstallError(f"invalid existing version member {existing}")
                        if candidate.exists() != existing.exists() or (
                            candidate.exists() and sha256(candidate) != sha256(existing)
                        ):
                            raise InstallError(f"existing {version_dir} differs from the verified archive")
                else:
                    os.replace(stage, version_dir)
                cli = version_dir / "rgo"
                record_path = cargo_home / ".rgo-install.json"
                state_path = install_root / "installer-state.json"
                if record_path.is_symlink():
                    raise InstallError(f"refusing symlinked activation record {record_path}")
                if state_path.is_symlink():
                    raise InstallError(f"refusing symlinked installer state {state_path}")
                state = json.loads(state_path.read_text()) if state_path.is_file() else None
                had_activation = record_path.is_file()
                previous_cli = None
                rollback_args = None
                if had_activation:
                    old_record = json.loads(record_path.read_text())
                    if (
                        not isinstance(old_record, dict)
                        or not isinstance(old_record.get("rgo_binary"), str)
                    ):
                        raise InstallError("existing activation has an invalid binary record")
                    previous_cli = Path(old_record["rgo_binary"]).resolve()
                    if (
                        not isinstance(state, dict)
                        or state.get("schema_version") != 1
                        or state.get("rgo_binary") != str(previous_cli)
                        or state.get("no_service") is not args.no_service
                        or state.get("supervised", False) is not args.supervised
                    ):
                        raise InstallError(
                            "existing activation has no matching installer state or service mode; "
                            "repair it before reinstalling"
                        )
                    if (
                        previous_cli.name != "rgo"
                        or not previous_cli.is_file()
                        or not previous_cli.is_relative_to(versions.resolve())
                        or old_record.get("install_root") != str(previous_cli.parent)
                    ):
                        raise InstallError("previous rgo binary is not an owned versioned install")
                    old_wrapper = previous_cli.with_name("rgo-rustc-wrapper")
                    if old_wrapper.is_symlink() or not old_wrapper.is_file():
                        raise InstallError("previous rgo wrapper is missing")
                    for name, target_path in (("rgo", previous_cli), ("rgo-rustc-wrapper", old_wrapper)):
                        link = bin_dir / name
                        if not link.is_symlink() or link.resolve() != target_path:
                            raise InstallError(f"owned command link changed: {link}")
                    old_version = old_record.get("binary_version")
                    if not isinstance(old_version, str) or run(
                        previous_cli, "--version", environment=environment
                    ).stdout.strip() != f"rgo {old_version}":
                        raise InstallError("previous rgo binary does not match its activation record")
                    old_protocol = old_record.get("protocol_version")
                    if not isinstance(old_protocol, int) or run(
                        old_wrapper, "--rgo-version", environment=environment
                    ).stdout.strip() != f"rgo-rustc-wrapper {old_version} protocol {old_protocol}":
                        raise InstallError("previous wrapper does not match its activation record")
                    if previous_cli != cli.resolve():
                        if not args.no_service:
                            raise InstallError(
                                "service-managed upgrades need a service rollback transaction; "
                                "the previous installation remains active"
                            )
                        rollback_args = ["setup", "--no-service"]
                        if args.supervised:
                            previous_shim = old_record.get("supervised_cargo")
                            if not isinstance(previous_shim, dict) or not isinstance(
                                previous_shim.get("real_cargo"), str
                            ):
                                raise InstallError("previous supervised Cargo proxy is not recorded")
                            rollback_args.extend(
                                ["--supervised", "--real-cargo", previous_shim["real_cargo"]]
                            )
                        elif old_record.get("wrapper_binary") is None:
                            rollback_args.append("--no-wrapper")
                activation_before = None
                activation_root = None
                entrypoints = (bin_dir / "rgo-rustc-wrapper", bin_dir / "rgo")
                if rollback_args is not None:
                    tracked = activation_paths(cargo_home, install_root, old_record)
                    activation_before = activation_snapshot(tracked)
                    activation_root = Path(old_record["rgo_home"])
                elif not had_activation and args.no_service:
                    if state_path.exists() or (cargo_home / ".rgo-home").exists():
                        raise InstallError("activation files exist without a record; repair them before installing")
                    activation_root = Path(
                        environment.get("RGO_HOME") or Path.home() / ".rgo"
                    ).expanduser().absolute()
                    environment["RGO_HOME"] = str(activation_root)
                    config_path = cargo_home / ("config" if (cargo_home / "config").exists() else "config.toml")
                    first_record = {
                        "config_file": str(config_path),
                        "rgo_home": str(activation_root),
                        "supervised_cargo": {"shim_path": str(cargo_home / "rgo/shims/cargo")}
                        if args.supervised else None,
                    }
                    activation_before = activation_snapshot(
                        activation_paths(cargo_home, install_root, first_record)
                    )
                setup_args = ["setup", *(["--no-service"] if args.no_service else [])]
                if args.supervised:
                    setup_args.extend(["--supervised", "--real-cargo", str(real_cargo)])
                previous_links = link_snapshot(entrypoints)
                if not had_activation and args.no_service and any(
                    target is not None for target in previous_links.values()
                ):
                    raise InstallError("owned command links exist without an activation record; repair them before installing")
                new_state = {
                    "schema_version": 1,
                    "rgo_binary": str(cli.resolve()),
                    "no_service": args.no_service,
                    "supervised": args.supervised,
                }
                journal = None
                if activation_before is not None:
                    journal = {
                        "schema_version": 1,
                        "phase": "prepared",
                        "first_install": not had_activation,
                        "cargo_home": str(cargo_home),
                        "install_root": str(install_root),
                        "rgo_home": str(activation_root),
                        "config_file": str(config_path) if not had_activation else None,
                        "supervised": args.supervised,
                        "before": encode_snapshot(activation_before),
                        "old_links": encode_links(previous_links),
                        "new_links": encode_links({
                            bin_dir / "rgo-rustc-wrapper": version_dir / "rgo-rustc-wrapper",
                            bin_dir / "rgo": cli,
                        }),
                    }
                setup_started = False
                setup_completed = False
                planned_after = None
                try:
                    run(cli, *setup_args, "--dry-run", environment=environment)
                    if journal is not None:
                        plan = json.loads(
                            run(cli, *setup_args, "--installer-plan-json", environment=environment).stdout
                        )
                        planned_after = planned_snapshot(
                            plan, activation_before, state_path, new_state
                        )
                        planned_record = planned_after[record_path]
                        if planned_record is None or Path(
                            json.loads(planned_record[0])["rgo_binary"]
                        ).resolve() != cli.resolve():
                            raise InstallError("replacement setup plan does not name the staged binary")
                        if activation_snapshot(tuple(activation_before)) != activation_before or link_snapshot(entrypoints) != previous_links:
                            raise InstallError("activation changed while preparing the installer journal")
                        journal["after"] = encode_snapshot(planned_after)
                        write_state(journal_path, journal)
                        if args.development_bundle and os.environ.get("RGO_INSTALLER_TEST_EXIT_AFTER_PREPARED") == "1":
                            os._exit(85)
                    setup_started = True
                    run(
                        cli,
                        *setup_args,
                        environment=environment,
                        test_abrupt_exit=args.development_bundle
                        and environment.get("RGO_SETUP_TEST_EXIT_AFTER_RECORD") == "1",
                    )
                    setup_completed = True
                    if activation_before is not None:
                        replacement = activation_snapshot(tuple(activation_before))
                        replacement[state_path] = (state_bytes(new_state), 0o600)
                        if replacement != planned_after:
                            raise InstallError("replacement setup differed from its recorded activation plan")
                        journal["phase"] = "setup_done"
                        write_state(journal_path, journal)
                        if args.development_bundle and os.environ.get("RGO_INSTALLER_TEST_EXIT_AFTER_SETUP_DONE") == "1":
                            os._exit(86)
                    verify_environment = environment.copy()
                    verify_environment.pop("RGO_HOME", None)
                    if args.supervised:
                        shim_dir = cargo_home / "rgo" / "shims"
                        verify_environment["PATH"] = f"{shim_dir}{os.pathsep}{environment.get('PATH', '')}"
                    doctor = run(cli, "doctor", "--verify", "--json", environment=verify_environment)
                    if json.loads(doctor.stdout).get("activation_verified") is not True:
                        raise InstallError("plain Cargo activation was not verified")
                    publish_entrypoint(bin_dir / "rgo-rustc-wrapper", version_dir / "rgo-rustc-wrapper")
                    publish_entrypoint(bin_dir / "rgo", cli)
                    write_state(state_path, new_state)
                    if journal is not None:
                        journal["phase"] = "committed"
                        write_state(journal_path, journal)
                        if args.development_bundle and os.environ.get("RGO_INSTALLER_TEST_EXIT_AFTER_COMMITTED") == "1":
                            os._exit(87)
                        try:
                            journal_path.unlink()
                        except OSError as cleanup_error:
                            print(f"warning: committed installer journal remains: {cleanup_error}", file=sys.stderr)
                except Exception as error:
                    rollback_errors = []
                    if planned_after is not None:
                        try:
                            if activation_snapshot(tuple(activation_before)) != activation_before:
                                restore_activation(
                                    cargo_home, activation_root, activation_before, planned_after
                                )
                        except Exception as rollback_error:
                            rollback_errors.append(str(rollback_error))
                    elif setup_started and not had_activation:
                        recovery_args = ["setup", "--undo", *(["--no-service"] if args.no_service else [])]
                        try:
                            run(cli, *recovery_args, environment=environment)
                        except Exception as rollback_error:
                            rollback_errors.append(str(rollback_error))
                    elif setup_started and rollback_args is not None and not setup_completed:
                        try:
                            run(previous_cli, *rollback_args, environment=environment)
                            restored = json.loads(record_path.read_text())
                            if Path(restored["rgo_binary"]).resolve() != previous_cli:
                                raise InstallError("rollback did not restore the previous activation")
                        except Exception as rollback_error:
                            rollback_errors.append(str(rollback_error))
                    elif setup_completed and rollback_args is not None:
                        rollback_errors.append("replacement activation changed before it could be snapshotted")
                    for path, previous in previous_links.items():
                        try:
                            restore_entrypoint(path, previous)
                        except Exception as rollback_error:
                            rollback_errors.append(f"restoring {path}: {rollback_error}")
                    if not had_activation and activation_before is not None:
                        try:
                            if activation_snapshot(tuple(activation_before)) != activation_before:
                                rollback_errors.append("first activation did not return to its prior files")
                        except Exception as rollback_error:
                            rollback_errors.append(str(rollback_error))
                    if rollback_errors:
                        raise InstallError(
                            f"activation failed ({error}); rollback incomplete "
                            f"({'; '.join(rollback_errors)}); verified binaries remain at {version_dir}"
                        ) from error
                    if journal is not None:
                        journal_path.unlink(missing_ok=True)
                    raise
                print(f"installed {args.version} at {version_dir}")
                print(f"rgo command: {bin_dir / 'rgo'}")
                if args.supervised:
                    activated = ensure_shell_activation(
                        install_root, cargo_home,
                        Path(environment.get("HOME") or Path.home()).expanduser().absolute(),
                        environment.get("SHELL", ""),
                        args.development_bundle,
                    )
                    if activated:
                        print("future login and interactive shell sessions will use supervised Cargo")
                    else:
                        print("shell startup files were not recognized; activate supervised Cargo manually")
                    print(f"activate supervised Cargo in this shell: export PATH={shlex.quote(str(cargo_home / 'rgo' / 'shims'))}:\"$PATH\"")
            finally:
                if stage.exists():
                    shutil.rmtree(stage)


if __name__ == "__main__":
    try:
        main()
    except (InstallError, OSError, subprocess.CalledProcessError, tarfile.TarError, ValueError) as error:
        print(f"rgo installer: {error}", file=sys.stderr)
        raise SystemExit(1)
