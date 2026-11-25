#!/usr/bin/env python3
"""Validate committed Cargo versions and stamp an archived build tree."""

import hashlib
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tomllib


ROOT = Path(__file__).resolve().parents[2]
SENTINEL = "0.0.0"
NUMBER = r"(?:0|[1-9][0-9]*)"
VERSION = re.compile(rf"(?P<base>{NUMBER}\.{NUMBER}\.{NUMBER})(?:-(?P<kind>beta|rc)\.[1-9][0-9]*)?")
CANDIDATE_VERSION = f"{SENTINEL}-rc.1"
CARGO_INPUTS = ("Cargo.toml", "Cargo.lock")
PRODUCTION = {
    "apps": "gh-docs-mcp",
    "crates/core": "gh-docs-core",
    "crates/git": "gh-docs-git",
    "crates/search": "gh-docs-search",
    "crates/storage": "gh-docs-storage",
    "crates/sync": "gh-docs-sync",
}
FIXTURE = {"crates/test-support": "gh-docs-test-support"}
VERSION_LINE = re.compile(r'(?m)^([ \t]*version[ \t]*=[ \t]*)"0\.0\.0"(?=[ \t]*(?:#[^\n]*)?$)')
WORKSPACE_PACKAGE = re.compile(r"(?ms)^\[workspace\.package\][ \t]*\n.*?(?=^\[|\Z)")
LOCK_PACKAGE = re.compile(r"(?m)(?=^\[\[package\]\][ \t]*$)")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def parse_version(version):
    match = VERSION.fullmatch(version) if len(version) <= 128 else None
    require(match, "expected a stable X.Y.Z, beta X.Y.Z-beta.N or candidate X.Y.Z-rc.N version")
    require(version != SENTINEL, f"{SENTINEL} is reserved for development builds")
    return match


def check_workspace(read):
    manifest, lock = (read(name) for name in CARGO_INPUTS)
    workspace = tomllib.loads(manifest)["workspace"]
    actual = workspace["package"]["version"]
    require(actual == SENTINEL, f"Cargo.toml workspace.package.version must be {SENTINEL}, got {actual!r}")
    members = {**PRODUCTION, **FIXTURE}
    require(sorted(workspace["members"]) == sorted(members), "update release version policy for Cargo workspace members")
    for member, name in members.items():
        path = f"{member}/Cargo.toml"
        package = tomllib.loads(read(path))["package"]
        expected = {"workspace": True} if member in PRODUCTION else SENTINEL
        require(package["name"] == name and package["version"] == expected,
                f"{path} must declare {name} with package version {expected!r}")
    packages = tomllib.loads(lock)["package"]
    for name in members.values():
        entries = [entry for entry in packages if entry["name"] == name and "source" not in entry]
        require(len(entries) == 1 and entries[0]["version"] == SENTINEL,
                f"Cargo.lock must contain one workspace entry for {name} at {SENTINEL}")
    return manifest, lock, packages


def replace_version(block, version, name):
    updated, count = VERSION_LINE.subn(lambda match: f'{match[1]}"{version}"', block)
    require(count == 1, f"expected one {SENTINEL} version line for {name}")
    return updated


def stamped_inputs(read, version):
    parse_version(version)
    manifest, lock, packages = check_workspace(read)
    workspace = WORKSPACE_PACKAGE.search(manifest)
    require(workspace, "expected a [workspace.package] table in Cargo.toml")
    updated = replace_version(workspace[0], version, "[workspace.package]")
    manifest = manifest[:workspace.start()] + updated + manifest[workspace.end():]
    header, *blocks = LOCK_PACKAGE.split(lock)
    require(len(blocks) == len(packages), "Cargo.lock package headers do not match parsed packages")
    for index, package in enumerate(packages):
        if package["name"] in PRODUCTION.values() and "source" not in package:
            blocks[index] = replace_version(blocks[index], version, package["name"])
    return dict(zip(CARGO_INPUTS, (manifest, header + "".join(blocks)), strict=True))


def stage_source(revision, version, work):
    require(re.fullmatch(r"[0-9a-f]{40}", revision), "expected a full Git source revision")
    source = work / "source"
    source.mkdir()
    archive = work / "source.tar"
    with archive.open("wb") as output:
        subprocess.run(["git", "-C", ROOT, "archive", "--format=tar", revision], stdout=output, check=True)
    with tarfile.open(archive) as data:
        data.extractall(source, filter="data")
    for name, content in stamped_inputs(lambda name: (source / name).read_text(), version).items():
        (source / name).write_text(content)
    return source


def cargo_digests(source):
    return {name: hashlib.sha256((source / name).read_bytes()).hexdigest() for name in CARGO_INPUTS}


if __name__ == "__main__":
    try:
        check_workspace(lambda name: (ROOT / name).read_text())
    except (KeyError, OSError, ValueError) as error:
        sys.exit(str(error))
