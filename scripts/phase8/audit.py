#!/usr/bin/env python3
"""Audit exact native release and infrastructure images and locked Rust inputs."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
from urllib.parse import unquote, urlsplit

from backup import sha256
from images import APP_ROLES, DOCKER, architecture, load_native, run, write_json
from pins import infrastructure
from release_version import CARGO_INPUTS, cargo_digests, stage_source


TRIVY = "ghcr.io/aquasecurity/trivy:0.76.2@sha256:8f6bb947dbc528902e11f2f5be52f7b519f6a39d5d148f41166f2040499768fb"
RIPGREP_REVISION = "669cb4c755db20b26c3af3d923c042ae224c7496"
RIPGREP = f"https://codeload.github.com/BurntSushi/ripgrep/tar.gz/{RIPGREP_REVISION}"
GIT = "https://www.kernel.org/pub/software/scm/git/git-2.54.1.tar.xz"
OXIBELT_REVISION = "e879ac7e263b0af9a86280cd8799de44cbffc05c"
OXIBELT_REPOSITORY = "OxiBelt/OxiBelt"
PROXY_PACKAGE = "oxibelt-dataplane-strict"


def validate_proxy_inventory(bom, lock):
    if bom.get("bomFormat") != "CycloneDX":
        raise ValueError("proxy inventory must be CycloneDX")
    root = bom["metadata"]["component"]
    components = [root, *bom.get("components", [])]
    by_ref = {}
    while components:
        component = components.pop()
        ref = component["bom-ref"]
        if ref in by_ref:
            raise ValueError("proxy inventory repeats a component identifier")
        by_ref[ref] = component
        components.extend(component.get("components", []))
    dependencies = bom.get("dependencies", [])
    graph = {entry["ref"]: entry.get("dependsOn", []) for entry in dependencies}
    if len(graph) != len(dependencies):
        raise ValueError("proxy inventory repeats a dependency graph node")
    if graph.keys() - by_ref.keys() or any(
        not isinstance(refs, list) or any(ref not in by_ref for ref in refs)
        for refs in graph.values()
    ):
        raise ValueError("proxy inventory contains unresolved dependency references")

    root_ref = root["bom-ref"]
    if root.get("type") == "application" and root["name"] == PROXY_PACKAGE:
        proxy = root_ref
    else:
        proxies = [ref for ref in graph.get(root_ref, [])
                   if by_ref[ref].get("type") == "application" and by_ref[ref]["name"] == PROXY_PACKAGE]
        if len(proxies) != 1:
            raise ValueError("proxy inventory lacks a unique strict data plane application")
        proxy = proxies[0]

    locked = {(item["name"], item["version"]) for item in lock["package"]}
    pending = [proxy]
    visited = set()
    cargo = set()
    while pending:
        ref = pending.pop()
        if ref in visited:
            continue
        visited.add(ref)
        component = by_ref[ref]
        purl = component.get("purl", "")
        if ref != proxy and purl.startswith("pkg:cargo/"):
            name, separator, version = urlsplit(purl).path.removeprefix("cargo/").rpartition("@")
            package = (unquote(name), unquote(version))
            if not separator or package not in locked:
                raise ValueError("proxy inventory contains a Cargo component outside the released lockfile")
            if package != (component["name"], component["version"]):
                raise ValueError("proxy inventory contains conflicting Cargo component identities")
            cargo.add(ref)
        pending.extend(graph.get(ref, []))
    if not cargo:
        raise ValueError("proxy inventory lacks a compiled Cargo dependency graph")
    if any(item.get("purl", "").startswith("pkg:cargo/") and ref not in visited
           for ref, item in by_ref.items()):
        raise ValueError("proxy inventory contains Cargo components outside the strict data plane graph")
    complete = {ref for entry in bom.get("compositions", []) if entry["aggregate"] == "complete"
                for ref in entry.get("dependencies", [])}
    if not {proxy, *cargo} <= complete:
        raise ValueError("proxy inventory does not mark the strict data plane dependency graph complete")


def proxy_sbom(ref, out, lock):
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
            run("gh", "attestation", "verify", f"oci://{subject}", "--repo", OXIBELT_REPOSITORY,
                "--source-digest", OXIBELT_REVISION, "--source-ref", f"refs/tags/{version}",
                "--deny-self-hosted-runners", "--predicate-type", predicate, "--format", "json",
                stdout=stream, timeout=120)
        results = json.loads(receipt.read_text())
        verified = [item["verificationResult"] for item in results]
        workflow = f"https://github.com/{OXIBELT_REPOSITORY}/.github/workflows/"
        accepted = {f"{workflow}{name}@refs/tags/{version}"
                    for name in ("release.yml", "release-image-arch.yml")}
        digest = subject.rsplit("@sha256:", 1)[1]
        statement = next(
            item["statement"] for item in verified
            if item["signature"]["certificate"]["subjectAlternativeName"] in accepted
            and item["statement"]["predicateType"] == predicate
            and any(entry["digest"].get("sha256") == digest for entry in item["statement"]["subject"])
        )
    bom = statement["predicate"]
    write_json(out / "proxy-upstream-sbom.json", bom)
    validate_proxy_inventory(bom, lock)


def download(url, expected, path):
    run("curl", "-fsSL", "--retry", "3", "--retry-all-errors", "--max-time", "90", url, "-o", path)
    if sha256(path) != expected:
        raise ValueError("source archive digest mismatch")


def supply_chain(out, source, ripgrep_lock, advisory_db):
    blockers = []
    proxy_lock = out / "oxibelt-Cargo.lock"
    with proxy_lock.open("w") as record:
        run("gh", "api", "-H", "Accept: application/vnd.github.raw+json",
            f"repos/{OXIBELT_REPOSITORY}/contents/Cargo.lock?ref={OXIBELT_REVISION}",
            stdout=record, timeout=60)
    for name, lock in (("workspace", source / "Cargo.lock"), ("ripgrep", ripgrep_lock),
                       ("oxibelt", proxy_lock)):
        args = ["cargo", "audit", "--file", lock, "--json", "--db", advisory_db, "--no-fetch"]
        with (out / f"{name}-rustsec.json").open("w") as receipt:
            audit = subprocess.run(args, stdout=receipt, check=False)
        report = json.loads((out / f"{name}-rustsec.json").read_text())
        count = report["vulnerabilities"]["count"]
        if audit.returncode not in (0, 1) or (audit.returncode == 1 and count == 0):
            raise ValueError(f"{name}: RustSec audit failed (exit {audit.returncode})")
        if count:
            blockers.append(f"{name}: RustSec vulnerabilities")
    with (out / "cargo-metadata.json").open("w") as metadata:
        run("cargo", "metadata", "--format-version", "1", "--frozen", cwd=source, stdout=metadata)
    return blockers


def exact_images(bundle, out, tag, work, proxy_lock):
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
            if role in APP_ROLES:
                run(*DOCKER, "run", "--rm", "--network", "none", "--read-only", "--cap-drop", "ALL",
                    "--security-opt", "no-new-privileges", "--entrypoint", "/bin/sh", ref, "-ec",
                    "test -f /usr/share/licenses/docs-mcp/LICENSE; "
                    "test -f /usr/share/licenses/docs-mcp/RFC-MCP-DOCKER-LICENSE; "
                    "for tool in cargo rustc make gcc python3 node pnpm; do ! command -v \"$tool\" || exit 1; done; "
                    "test -z \"$(find /usr/local/bin -name '*fixture*')\"; "
                    "test ! -e /run/secrets && test ! -e /src")
            if role == "postgres":
                options = (*DOCKER, "run", "--rm", "--network", "none", "--read-only", "--cap-drop", "ALL",
                           "--security-opt", "no-new-privileges")
                run(*options, ref, "/bin/sh", "-ec",
                    'test "$(id -u):$(id -g)" = 70:70; test ! -e /usr/local/bin/gosu; '
                    'postgres --version')
                root = subprocess.run([*options, "--user", "0:0", ref], capture_output=True, text=True)
                if root.returncode != 1 or root.stderr.strip() != "PostgreSQL requires UID and GID 70":
                    raise ValueError("PostgreSQL image did not reject root startup")
            saved = work / "image.tar"
            # Use an image tag so the archive contains an explicit Docker reference.
            name = f"docs-mcp-{role}:{tag}" if role in refs else ref
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
                    proxy_sbom(ref, out, proxy_lock)
                    run(*scanner, "sbom", "--skip-db-update", "--format", "json",
                        "--output", "/audit/proxy-upstream-vulnerabilities.json", "/audit/proxy-upstream-sbom.json")
                    upstream = json.loads((out / "proxy-upstream-vulnerabilities.json").read_text())
                    report["Results"] = (report.get("Results") or []) + (upstream.get("Results") or [])
                except (AttributeError, TypeError, OSError, ValueError, KeyError, StopIteration,
                        subprocess.SubprocessError) as error:
                    reason = str(error) or "no matching signed attestation"
                    print(f"proxy inventory verification failed: {reason}", flush=True)
                    blockers.append(f"proxy: signed native SBOM audit unavailable ({reason})")
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
    with tempfile.TemporaryDirectory(prefix="docsmcp8-audit-", dir="/tmp") as directory:
        work = Path(directory)
        receipt = json.loads((args.bundle / "receipt.json").read_bytes())
        workspace = stage_source(receipt["revision"], receipt["version"], work)
        if cargo_digests(workspace) != receipt["cargo_inputs"]:
            raise ValueError("stamped Cargo inputs do not match the native build receipt")
        cargo_evidence = out / "cargo-inputs"
        cargo_evidence.mkdir()
        for name in CARGO_INPUTS:
            shutil.copyfile(workspace / name, cargo_evidence / name)
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
        blockers = supply_chain(out, workspace, lock, advisory_db)
        proxy_lock = tomllib.loads((out / "oxibelt-Cargo.lock").read_text())
        blockers += exact_images(args.bundle, out, args.tag, work, proxy_lock)
    write_json(out / "disposition.json", {"blockers": sorted(set(blockers))})
    if blockers:
        raise SystemExit("release audit requires disposition of findings")


if __name__ == "__main__":
    main()
