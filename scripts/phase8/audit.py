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
from images import DOCKER, architecture, load_native, run, write_json
from pins import infrastructure
from release_version import CARGO_INPUTS, cargo_digests, stage_source


TRIVY = "ghcr.io/aquasecurity/trivy:0.76.2@sha256:8f6bb947dbc528902e11f2f5be52f7b519f6a39d5d148f41166f2040499768fb"
RIPGREP_REVISION = "669cb4c755db20b26c3af3d923c042ae224c7496"
RIPGREP = f"https://codeload.github.com/BurntSushi/ripgrep/tar.gz/{RIPGREP_REVISION}"
GIT = "https://www.kernel.org/pub/software/scm/git/git-2.54.1.tar.xz"
OXIBELT_REVISION = "e879ac7e263b0af9a86280cd8799de44cbffc05c"


def proxy_sbom(ref, out):
    index = json.loads(subprocess.check_output(
        [*DOCKER, "buildx", "imagetools", "inspect", "--raw", ref], timeout=90))
    child, = (item for item in index["manifests"]
              if item["platform"].get("os") == "linux"
              and item["platform"].get("architecture") == architecture())
    repository, tag = ref.split("@", 1)[0].rsplit(":", 1)
    version = tag.removesuffix("-alpine-musl")
    native = f"{repository}@{child['digest']}"
    for subject, predicate, filename in (
        (ref, "https://slsa.dev/provenance/v1", "proxy-index-provenance.json"),
        (native, "https://cyclonedx.org/bom", "proxy-native-sbom-attestation.json"),
    ):
        receipt = out / filename
        with receipt.open("w") as stream:
            run("gh", "attestation", "verify", f"oci://{subject}", "--repo", "OxiBelt/OxiBelt",
                "--source-digest", OXIBELT_REVISION, "--source-ref", f"refs/tags/{version}",
                "--deny-self-hosted-runners", "--predicate-type", predicate, "--format", "json",
                stdout=stream, timeout=120)
        results = json.loads(receipt.read_text())
        verified = [item["verificationResult"] for item in results]
        workflow = "https://github.com/OxiBelt/OxiBelt/.github/workflows/"
        accepted = {f"{workflow}{name}@refs/tags/{version}"
                    for name in ("release.yml", "release-image-arch.yml")}
        statement = next(item["statement"] for item in verified
                         if item["signature"]["certificate"]["subjectAlternativeName"] in accepted)
    bom = statement["predicate"]
    if bom.get("bomFormat") != "CycloneDX" or not bom.get("components"):
        raise ValueError("proxy upstream SBOM is empty or unsupported")
    write_json(out / "proxy-upstream-sbom.json", bom)


def download(url, expected, path):
    run("curl", "-fsSL", "--retry", "3", "--retry-all-errors", "--max-time", "90", url, "-o", path)
    if sha256(path) != expected:
        raise ValueError("source archive digest mismatch")


def supply_chain(out, source, ripgrep_lock, advisory_db):
    blockers = []
    proxy_lock = out / "oxibelt-Cargo.lock"
    with proxy_lock.open("w") as record:
        run("gh", "api", "-H", "Accept: application/vnd.github.raw+json",
            f"repos/OxiBelt/OxiBelt/contents/Cargo.lock?ref={OXIBELT_REVISION}",
            stdout=record, timeout=60)
    for name, lock in (("workspace", source / "Cargo.lock"), ("ripgrep", ripgrep_lock),
                       ("oxibelt", proxy_lock)):
        args = ["cargo", "audit", "--file", lock, "--json", "--db", advisory_db, "--no-fetch"]
        with (out / f"{name}-rustsec.json").open("w") as receipt:
            subprocess.run(args, stdout=receipt, check=False)
        report = json.loads((out / f"{name}-rustsec.json").read_text())
        if report["vulnerabilities"]["count"]:
            blockers.append(f"{name}: RustSec vulnerabilities")
    with (out / "cargo-metadata.json").open("w") as metadata:
        run("cargo", "metadata", "--format-version", "1", "--frozen", cwd=source, stdout=metadata)
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
            if role == "proxy":
                # The strict image omits its package database. Use its attested native inventory.
                try:
                    proxy_sbom(ref, out)
                    run(*scanner, "sbom", "--skip-db-update", "--format", "json",
                        "--output", "/audit/proxy-upstream-vulnerabilities.json", "/audit/proxy-upstream-sbom.json")
                    upstream = json.loads((out / "proxy-upstream-vulnerabilities.json").read_text())
                    report["Results"] = (report.get("Results") or []) + (upstream.get("Results") or [])
                except (OSError, ValueError, KeyError, StopIteration, subprocess.SubprocessError) as error:
                    print(f"proxy inventory verification failed: {error}", flush=True)
                    blockers.append("proxy: signed native SBOM audit unavailable")
            for result in report.get("Results") or []:
                for finding in result.get("Vulnerabilities") or []:
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
        receipt = json.loads((args.bundle / "receipt.json").read_bytes())
        source = stage_source(receipt["revision"], receipt["version"], work)
        if cargo_digests(source) != receipt["cargo_inputs"]:
            raise ValueError("stamped Cargo inputs do not match the native build receipt")
        cargo_evidence = out / "cargo-inputs"
        cargo_evidence.mkdir()
        for name in CARGO_INPUTS:
            shutil.copyfile(source / name, cargo_evidence / name)
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
        blockers = supply_chain(out, source, lock, advisory_db)
        blockers += exact_images(args.bundle, out, args.tag, work)
    write_json(out / "disposition.json", {"blockers": sorted(set(blockers))})
    if blockers:
        raise SystemExit("release audit requires disposition of findings")


if __name__ == "__main__":
    main()
