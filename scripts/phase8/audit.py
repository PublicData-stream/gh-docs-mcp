#!/usr/bin/env python3
"""Audit exact native release and infrastructure images and locked Rust inputs."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

from backup import sha256
from images import DOCKER, ROOT, load_native, run, write_json
from pins import infrastructure


TRIVY = "ghcr.io/aquasecurity/trivy:0.76.2@sha256:8f6bb947dbc528902e11f2f5be52f7b519f6a39d5d148f41166f2040499768fb"
RIPGREP_REVISION = "669cb4c755db20b26c3af3d923c042ae224c7496"
RIPGREP = f"https://codeload.github.com/BurntSushi/ripgrep/tar.gz/{RIPGREP_REVISION}"
GIT = "https://www.kernel.org/pub/software/scm/git/git-2.54.1.tar.xz"


def download(url, expected, path):
    run("curl", "-fsSL", "--retry", "3", "--retry-all-errors", "--max-time", "90", url, "-o", path)
    if sha256(path) != expected:
        raise ValueError("source archive digest mismatch")


def supply_chain(out, ripgrep_lock, advisory_db):
    blockers = []
    for name, lock in (("workspace", ROOT / "Cargo.lock"), ("ripgrep", ripgrep_lock)):
        args = ["cargo", "audit", "--file", lock, "--json", "--db", advisory_db, "--no-fetch"]
        with (out / f"{name}-rustsec.json").open("w") as receipt:
            subprocess.run(args, stdout=receipt, check=False)
        report = json.loads((out / f"{name}-rustsec.json").read_text())
        if report["vulnerabilities"]["count"]:
            blockers.append(f"{name}: RustSec vulnerabilities")
    with (out / "cargo-metadata.json").open("w") as metadata:
        run("cargo", "metadata", "--format-version", "1", "--frozen", stdout=metadata)
    return blockers


def exact_images(bundle, out, tag, work):
    infra = infrastructure()
    del infra["fixture-proxy"]
    scanner = [*DOCKER, "run", "--rm", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
               "--user", f"{os.getuid()}:{os.getgid()}", "--env", "TRIVY_SKIP_VERSION_CHECK=true",
               "--mount", f"type=bind,src={out},dst=/audit", "--mount", f"type=bind,src={work},dst=/work",
               TRIVY, "--cache-dir", "/work/cache"]
    run(*scanner, "image", "--download-db-only")
    with (out / "trivy-version.json").open("w") as report:
        run(*scanner, "version", "--format", "json", stdout=report)
    blockers = []
    with load_native(bundle, tag) as refs:
        candidates = {**refs, **infra}
        write_json(out / "images.json", candidates)
        for role, ref in candidates.items():
            if role in refs:
                run(*DOCKER, "run", "--rm", "--network", "none", "--read-only", "--cap-drop", "ALL",
                    "--security-opt", "no-new-privileges", "--entrypoint", "/bin/sh", ref, "-ec",
                    "test -f /usr/share/licenses/gh-docs-mcp/LICENSE; "
                    "test -f /usr/share/licenses/gh-docs-mcp/RFC-MCP-DOCKER-LICENSE; "
                    "for tool in cargo rustc make gcc python3 node pnpm; do ! command -v \"$tool\" || exit 1; done; "
                    "test -z \"$(find /usr/local/bin -name '*fixture*')\"; "
                    "test ! -e /run/secrets && test ! -e /src")
            saved = work / "image.tar"
            # Use an image tag so the archive contains an explicit Docker reference.
            name = f"gh-docs-mcp-{role}:{tag}" if role in refs else ref
            run(*DOCKER, "image", "save", "-o", saved, name)
            for file, format_name in (("sbom", "cyclonedx"), ("vulnerabilities", "json")):
                options = ["--scanners", "vuln"] if file == "vulnerabilities" else []
                run(*scanner, "image", "--input", "/work/image.tar", "--skip-db-update",
                    "--format", format_name, "--output", f"/audit/{role}-{file}.json", *options)
            saved.unlink()
            report = json.loads((out / f"{role}-vulnerabilities.json").read_text())
            for result in report.get("Results", []):
                for finding in result.get("Vulnerabilities", []):
                    if finding.get("Severity") in ("HIGH", "CRITICAL"):
                        blockers.append(f"{role}: {finding['VulnerabilityID']} {finding['PkgName']}")
    return blockers


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--advisory-db", type=Path)
    parser.add_argument("--git-source", type=Path, help="local copy of the pinned Git source archive")
    args = parser.parse_args()
    out = args.output.resolve()
    if not out.is_relative_to("/tmp"):
        parser.error("audit output must be under /tmp")
    os.umask(0o077)
    out.mkdir()
    with tempfile.TemporaryDirectory(prefix="ghdocs8-audit-", dir="/tmp") as directory:
        work = Path(directory)
        ripgrep = work / "ripgrep.tar.gz"
        download(RIPGREP, "21002cba547651acf2ffb2f1e90010ee9533df3ce8819b171378d55a4f5953fd", ripgrep)
        lock = out / "ripgrep-Cargo.lock"
        with lock.open("wb") as record:
            run("tar", "-xOzf", ripgrep, f"ripgrep-{RIPGREP_REVISION}/Cargo.lock", stdout=record)
        git_source = out / "git-2.54.1.tar.xz"
        git_digest = "ca988eb9af7d8770339df7cdc38106fec8244664ff886840767cb9be3ef0e16c"
        if args.git_source:
            if sha256(args.git_source) != git_digest:
                raise ValueError("Git source archive digest mismatch")
            with args.git_source.open("rb") as source, git_source.open("wb") as target:
                shutil.copyfileobj(source, target)
        else:
            download(GIT, git_digest, git_source)
        advisory_db = args.advisory_db
        if advisory_db is None:
            advisory_db = work / "advisory-db"
            run("git", "clone", "--depth", "1", "https://github.com/RustSec/advisory-db.git", advisory_db)
        with (out / "rustsec-revision.txt").open("w") as record:
            run("git", "-C", advisory_db, "rev-parse", "HEAD", stdout=record)
        blockers = supply_chain(out, lock, advisory_db)
        blockers += exact_images(args.bundle, out, args.tag, work)
    write_json(out / "disposition.json", {"blockers": sorted(set(blockers))})
    if blockers:
        raise SystemExit("release audit requires disposition of findings")


if __name__ == "__main__":
    main()
