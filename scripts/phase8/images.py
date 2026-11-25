#!/usr/bin/env python3
"""Build, inspect, combine, and load qualified OCI release images."""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import time
from urllib.request import urlopen

from backup import sha256


ROOT = Path(__file__).resolve().parents[2]
SOURCE = "https://github.com/PublicData-stream/gh-docs-mcp"
CANDIDATE_VERSION = "0.1.0-rc.1"
DOCKER = ("docker", "--context", "default")
ROLES = ("serve", "sync", "feed")
ARCHES = ("amd64", "arm64")
BUILDKIT = "docker.io/moby/buildkit:v0.35.1@sha256:baf6138eb8c37644800c7c913fba051e840ebb9ba13eaf92d92ef9a1712d0b4c"
REGISTRY = "docker.io/library/registry:3@sha256:82f6ff4a7f982960c8aa51617fea3aabc485df6e55cce80d22deb1de36f1bc3c"
SBOM = "docker.io/docker/buildkit-syft-scanner:stable-1@sha256:873a7458c3c8ac8e32c7b796c10e1600b07b963d2aa000a72a2d97aaaa2d6315"
INDEX = "application/vnd.oci.image.index.v1+json"
VERSION = re.compile(r"(?:0|[1-9][0-9]*)(?:\.(?:0|[1-9][0-9]*)){2}(?:-(?:beta|rc)\.[1-9][0-9]*)?")


def run(*args, **kwargs):
    print("+ " + " ".join(map(str, args)), flush=True)
    return subprocess.run(args, check=True, **kwargs)


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def architecture():
    return {"x86_64": "amd64", "aarch64": "arm64"}[platform.machine()]


def write_json(path, value):
    path.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")


def blob_path(layout, digest):
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        raise ValueError("unsupported OCI digest")
    return layout / "blobs/sha256" / digest[7:]


def read_blob(layout, descriptor):
    path = blob_path(layout, descriptor["digest"])
    if path.stat().st_size != descriptor["size"] or sha256(path) != descriptor["digest"][7:]:
        raise ValueError("OCI blob does not match descriptor")
    return path.read_bytes()


def descriptors(layout):
    entries = json.loads((layout / "index.json").read_bytes())["manifests"]
    while len(entries) == 1 and entries[0]["mediaType"] == INDEX:
        entries = json.loads(read_blob(layout, entries[0]))["manifests"]
    return entries


def inspect(layout, arch):
    entries = descriptors(layout)
    runtime, = (item for item in entries if item.get("platform", {}).get("os") == "linux"
                and item["platform"]["architecture"] == arch)
    runtime_digest = runtime["digest"]
    attestations = set()
    for item in entries:
        image = json.loads(read_blob(layout, item))
        config = json.loads(read_blob(layout, image["config"]))
        annotations = item.get("annotations", {})
        if item is runtime:
            if config.get("architecture") != arch or config.get("os") != "linux":
                raise ValueError("runtime config differs from its platform descriptor")
        elif (annotations.get("vnd.docker.reference.type") != "attestation-manifest"
              or annotations.get("vnd.docker.reference.digest") != runtime_digest):
            raise ValueError("unexpected descriptor in native image bundle")
        for layer in image["layers"]:
            path = blob_path(layout, layer["digest"])
            if path.stat().st_size != layer["size"] or sha256(path) != layer["digest"][7:]:
                raise ValueError("OCI layer does not match descriptor")
            kind = layer.get("annotations", {}).get("in-toto.io/predicate-type")
            if item is not runtime and kind:
                statement = json.loads(path.read_bytes())
                if not any(subject.get("digest", {}).get("sha256") == runtime_digest[7:]
                           for subject in statement["subject"]):
                    raise ValueError("attestation subject differs from the runtime manifest")
                attestations.add(kind)
    if "https://spdx.dev/Document" not in attestations or not (
        {"https://slsa.dev/provenance/v1", "https://slsa.dev/provenance/v0.2"} & attestations
    ):
        raise ValueError("runtime manifest is missing provenance or SPDX SBOM")
    return runtime


def merge(layouts, destination, *, annotations=None):
    destination.mkdir()
    blobs = destination / "blobs/sha256"
    blobs.mkdir(parents=True)
    all_entries = []
    for layout in layouts:
        for source in (layout / "blobs/sha256").iterdir():
            target = blobs / source.name
            if not target.exists():
                shutil.copyfile(source, target)
        all_entries.extend(descriptors(layout))
    # The layout root references a real, digest-addressed deployment index.
    contents = {"schemaVersion": 2, "mediaType": INDEX, "manifests": all_entries}
    if annotations is not None:
        contents["annotations"] = annotations
    raw = json.dumps(contents, sort_keys=True, separators=(",", ":")).encode()
    digest = "sha256:" + hashlib.sha256(raw).hexdigest()
    blob_path(destination, digest).write_bytes(raw)
    write_json(destination / "index.json", {"schemaVersion": 2, "manifests": [{
        "mediaType": INDEX, "digest": digest, "size": len(raw),
        "annotations": {"org.opencontainers.image.ref.name": "release"},
    }]})
    write_json(destination / "oci-layout", {"imageLayoutVersion": "1.0.0"})
    return digest


@contextmanager
def builder():
    name = f"ghdocs8-{os.getpid()}"
    run(*DOCKER, "buildx", "create", "--name", name, "--driver", "docker-container",
        "--driver-opt", f"image={BUILDKIT},network=host", "--buildkitd-flags",
        "--allow-insecure-entitlement network.host")
    try:
        yield name
    finally:
        run(*DOCKER, "buildx", "rm", name)


def image_labels(revision, version):
    return {"org.opencontainers.image.source": SOURCE,
            "org.opencontainers.image.revision": revision,
            "org.opencontainers.image.version": version}


def build(destination, revision, seed, repeat, version=CANDIDATE_VERSION):
    if len(version) > 128 or not VERSION.fullmatch(version):
        raise ValueError("invalid OCI image version")
    arch = architecture()
    revision = output("git", "-C", ROOT, "rev-parse", f"{revision}^{{commit}}")
    epoch = output("git", "-C", ROOT, "show", "-s", "--format=%ct", revision)
    destination.mkdir(mode=0o700)
    with tempfile.TemporaryDirectory(prefix="ghdocs8-source-", dir="/tmp") as directory:
        work = Path(directory)
        archive = work / "source.tar"
        with archive.open("wb") as data:
            run("git", "-C", ROOT, "archive", "--format=tar", revision, stdout=data)
        source = work / "source"
        source.mkdir()
        with tarfile.open(archive) as data:
            data.extractall(source, filter="data")
        labels = image_labels(revision, version)
        with builder() as name:
            # A reproducibility check uses separate BuildKit state.
            for role in ROLES:
                export(name, source, work, destination / role, role, arch, epoch, labels, seed)
        result = {role: inspect(destination / role, arch)["digest"] for role in ROLES}
        if repeat:
            with builder() as name:
                for role in ROLES:
                    second = work / f"repeat-{role}"
                    export(name, source, work, second, role, arch, epoch, labels, seed)
                    actual = inspect(second, arch)["digest"]
                    if actual != result[role]:
                        raise ValueError(f"{role} did not reproduce its runtime manifest: {result[role]} != {actual}")
    write_json(destination / "receipt.json", {"source": SOURCE, "revision": revision, "version": version,
                                             "platform": arch, "images": result,
                                             "source_date_epoch": int(epoch), "repeated": repeat})


def export(builder_name, source, work, destination, role, arch, epoch, labels, seed):
    archive = work / "image.tar"
    args = [
        *DOCKER, "buildx", "build", "--builder", builder_name, "--platform", f"linux/{arch}",
        "--network", "host", "--allow", "network.host", "--target", role,
        "--build-arg", f"SOURCE_DATE_EPOCH={epoch}",
        "--tag", f"gh-docs-mcp-{role}:{labels['org.opencontainers.image.version']}",
        "--provenance", "mode=min", "--attest", f"type=sbom,generator={SBOM}",
        "--output", f"type=oci,dest={archive},rewrite-timestamp=true",
    ]
    for key, value in labels.items():
        args += ["--label", f"{key}={value}"]
    if seed:
        args += ["--build-context", f"cargo-registry-seed={seed}"]
    run(*args, source)
    destination.mkdir()
    with tarfile.open(archive) as data:
        data.extractall(destination, filter="data")
    archive.unlink()


def assemble(amd64, arm64, destination, *, revision=None, version=None):
    inputs = (amd64, arm64)
    receipts = [json.loads((directory / "receipt.json").read_text()) for directory in inputs]
    expected_revision = receipts[0]["revision"] if revision is None else revision
    expected_version = receipts[0]["version"] if version is None else version
    for directory, receipt, arch in zip(inputs, receipts, ARCHES, strict=True):
        if (receipt["platform"] != arch or not receipt["repeated"]
                or receipt["source"] != SOURCE or set(receipt["images"]) != set(ROLES)
                or receipt["revision"] != expected_revision
                or receipt["version"] != expected_version
                or receipt["source_date_epoch"] != receipts[0]["source_date_epoch"]):
            raise ValueError("release input receipts do not describe the same reproducible release")
        gate = json.loads((directory / "qualification.json").read_text())
        if gate != {"platform": arch, "revision": receipt["revision"], "images": receipt["images"],
                    "result": "pass"}:
            raise ValueError("required native qualification receipt is missing")
    destination.mkdir(mode=0o700)
    images = {}
    for role in ROLES:
        layouts = [path / role for path in inputs]
        manifests = {arch: inspect(layout, arch)["digest"]
                     for arch, layout in zip(ARCHES, layouts, strict=True)}
        if manifests != {receipt["platform"]: receipt["images"][role] for receipt in receipts}:
            raise ValueError("native receipt does not match the qualified manifest")
        index = merge(layouts, destination / role,
                      annotations=image_labels(expected_revision, expected_version))
        images[role] = {"index": index, "platforms": manifests}
    write_json(destination / "release.json", {"revision": expected_revision,
                                             "version": expected_version, "images": images})


@contextmanager
def local_registry():
    name = f"ghdocs8-registry-{os.getpid()}"
    run(*DOCKER, "run", "--detach", "--name", name, "--publish", "127.0.0.1::5000",
        "--tmpfs", "/var/lib/registry:size=2g", "--log-opt", "max-size=1m", REGISTRY)
    try:
        address = output(*DOCKER, "port", name, "5000/tcp")
        for _ in range(60):
            try:
                with urlopen(f"http://{address}/v2/", timeout=1):
                    break
            except OSError:
                time.sleep(0.25)
        else:
            raise RuntimeError("loopback registry did not become ready")
        yield address
    finally:
        run(*DOCKER, "rm", "--force", "--volumes", name)


@contextmanager
def load_native(bundle, tag):
    arch = architecture()
    refs = {}
    aliases = []
    for role in ROLES:
        alias = f"gh-docs-mcp-{role}:{tag}"
        existing = subprocess.run([*DOCKER, "image", "inspect", alias],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        if existing.returncode == 0:
            raise ValueError(f"qualification image tag already exists: {alias}")
    with local_registry() as registry, tempfile.TemporaryDirectory(dir="/tmp", prefix="ghdocs8-oci-") as directory:
        try:
            for role in ROLES:
                source = bundle / role
                digest = inspect(source, arch)["digest"]
                layout = Path(directory) / role
                merge([source], layout)
                repo = f"{registry}/gh-docs-mcp-{role}"
                run("skopeo", "copy", "--all", "--preserve-digests", "--dest-tls-verify=false",
                    f"oci:{layout}:release", f"docker://{repo}:{tag}")
                ref = f"{repo}@{digest}"
                refs[role] = ref
                run(*DOCKER, "pull", "--platform", f"linux/{arch}", ref)
                alias = f"gh-docs-mcp-{role}:{tag}"
                run(*DOCKER, "tag", ref, alias)
                aliases.append(alias)
            yield refs
        finally:
            for ref in [*aliases, *refs.values()]:
                subprocess.run([*DOCKER, "image", "rm", ref], check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    build_args = sub.add_parser("build")
    build_args.add_argument("destination", type=Path)
    build_args.add_argument("--revision", default="HEAD")
    build_args.add_argument("--version", default=CANDIDATE_VERSION)
    build_args.add_argument("--seed", type=Path)
    build_args.add_argument("--repeat", action="store_true")
    assemble_args = sub.add_parser("assemble")
    for name in (*ARCHES, "destination"):
        assemble_args.add_argument(name, type=Path)
    assemble_args.add_argument("--revision")
    assemble_args.add_argument("--version")
    args = parser.parse_args()
    if args.action == "build":
        build(args.destination.resolve(), args.revision, args.seed, args.repeat, args.version)
    else:
        assemble(args.amd64.resolve(), args.arm64.resolve(), args.destination.resolve(),
                 revision=args.revision, version=args.version)


if __name__ == "__main__":
    main()
