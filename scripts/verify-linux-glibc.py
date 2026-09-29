#!/usr/bin/env python3
"""Reject release binaries requiring glibc newer than the advertised floor."""

import argparse
import re
import subprocess
from pathlib import Path


VERSION = re.compile(r"\bGLIBC_(\d+)\.(\d+)(?:\.(\d+))?\b")


def readelf(path: Path, *args: str) -> str:
    return subprocess.run(
        ["readelf", "--wide", *args, str(path)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


def required_glibc(path: Path) -> tuple[int, int, int]:
    header = readelf(path, "--file-header")
    if not re.search(r"^\s*Class:\s*ELF64\s*$", header, re.MULTILINE):
        raise ValueError(f"{path}: expected a 64-bit ELF binary")
    if not re.search(
        r"^\s*Machine:\s*Advanced Micro Devices X86-64\s*$", header, re.MULTILINE
    ):
        raise ValueError(f"{path}: expected an x86_64 ELF binary")

    symbols = readelf(path, "--version-info")
    versions = {
        (int(major), int(minor), int(patch or 0))
        for major, minor, patch in VERSION.findall(symbols)
    }
    if not versions:
        raise ValueError(f"{path}: no glibc symbol versions found; baseline unverified")
    if "GLIBC_PRIVATE" in symbols:
        raise ValueError(f"{path}: uses a private glibc symbol")
    return max(versions)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--max-glibc", required=True, help="oldest supported glibc, e.g. 2.35")
    parser.add_argument("binaries", nargs="+", type=Path)
    args = parser.parse_args()
    ceiling = tuple(int(part) for part in args.max_glibc.split("."))
    if len(ceiling) != 2:
        parser.error("--max-glibc must be major.minor")
    for path in args.binaries:
        version = required_glibc(path)
        if version > (*ceiling, 0):
            raise SystemExit(
                f"{path}: requires glibc {version[0]}.{version[1]}.{version[2]}, "
                f"above supported {args.max_glibc}"
            )
        print(f"{path}: maximum required glibc {version[0]}.{version[1]}.{version[2]}")


if __name__ == "__main__":
    main()
