#!/usr/bin/env python3
"""Publish and verify the exact qualified OCI indexes."""

# Registry publication and promotion adapted from rfc-mcp-docker.
# See docs/RFC-MCP-DOCKER-LICENSE.

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from images import ARCHES, INDEX, ROLES, SOURCE, image_labels, inspect_entries, read_blob, sha256, write_json
from release_source import SHA, release_tag


NAMESPACE = "ghcr.io/publicdata-stream"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(raw):
    return "sha256:" + hashlib.sha256(raw).hexdigest()


def version_tuple(version):
    require(isinstance(version, str), "expected a stable OCI version")
    base, kind = release_tag(version)
    require(kind == "stable", "expected a stable OCI version")
    return tuple(map(int, base.split(".")))


class Registry:
    def __init__(self, namespace, authfile, *, anonymous=False):
        self.namespace = namespace
        self.authfile = authfile
        self.anonymous = anonymous

    def options(self, prefix=""):
        args = [f"--{prefix}authfile", str(self.authfile)]
        if self.namespace != NAMESPACE:
            args.append(f"--{prefix}tls-verify=false")
        if self.anonymous:
            args.append(f"--{prefix}no-creds")
        return args

    def manifest(self, ref, *, missing=False, bootstrap=False):
        result = subprocess.run(["skopeo", "inspect", "--raw", *self.options(), f"docker://{ref}"],
                                capture_output=True, timeout=90, check=False)
        if result.returncode == 0:
            return result.stdout
        error = result.stderr.decode(errors="replace").strip()
        if missing and re.search(r"\b(?:manifest unknown|name unknown)\b", error, re.I):
            return None
        # Before a GHCR package exists it may deny a token for an inspect.
        # Probe creation by digest, then require a definitive tag lookup.
        if bootstrap and self.namespace == NAMESPACE and re.search(
                r"Requesting bearer token: .*\b403 Forbidden\b", error, re.I):
            return "bootstrap"
        raise ValueError(f"cannot inspect {ref}: {error}")

    def copy(self, source, destination):
        options = []
        if source.startswith("docker://"):
            options.extend(self.options("src-"))
        if destination.startswith("docker://"):
            options.extend(self.options("dest-"))
        subprocess.run(["skopeo", "copy", "--all", "--preserve-digests", *options, source, destination],
                       check=True, timeout=480)

    def verify_digest(self, ref, expected):
        require(digest(self.manifest(ref)) == expected, f"registry index mismatch for {ref}")


def check_bundle_checksums(bundle):
    paths = list(bundle.rglob("*"))
    require(all(not path.is_symlink() and (path.is_file() or path.is_dir()) for path in paths),
            "invalid file in release bundle")
    checksums = bundle / "SHA256SUMS"
    entries = {}
    for line in checksums.read_text().splitlines():
        checksum, separator, filename = line.partition("  ./")
        require(separator and re.fullmatch(r"[0-9a-f]{64}", checksum)
                and filename and filename not in entries, "invalid release checksums")
        entries[filename] = checksum
    files = {str(path.relative_to(bundle)): path for path in paths if path.is_file() and path != checksums}
    require(entries.keys() == files.keys(), "release checksum inventory differs from the bundle")
    for filename, path in files.items():
        require(sha256(path) == entries[filename], f"release checksum mismatch for {filename}")


def release_entries(receipt, revision, version, namespace, *, published=False):
    require(receipt["revision"] == revision and receipt["version"] == version
            and set(receipt["images"]) == set(ROLES), "receipt describes a different release")
    images = {}
    for role in ROLES:
        entry = receipt["images"][role]
        repository = f"{namespace}/docs-mcp-{role}"
        require(DIGEST.fullmatch(entry["index"]) and set(entry["platforms"]) == set(ARCHES)
                and all(DIGEST.fullmatch(value) for value in entry["platforms"].values())
                and (not published or entry["image"] == repository), f"invalid release receipt for {role}")
        images[role] = {"image": repository, "index": entry["index"], "platforms": entry["platforms"]}
    return images


def check_layout(layout, expected, labels):
    marker = json.loads((layout / "oci-layout").read_bytes())
    require(marker["imageLayoutVersion"] == "1.0.0", f"invalid OCI layout for {expected['image']}")
    root = json.loads((layout / "index.json").read_bytes())
    descriptor, = root["manifests"]
    require(root["schemaVersion"] == 2 and descriptor["mediaType"] == INDEX
            and descriptor["digest"] == expected["index"]
            and descriptor["annotations"]["org.opencontainers.image.ref.name"] == "release",
            f"layout index digest or reference differs for {expected['image']}")
    index = json.loads(read_blob(layout, descriptor))
    require(index["schemaVersion"] == 2 and index["mediaType"] == INDEX
            and all(index["annotations"].get(key) == value for key, value in labels.items()),
            f"OCI index metadata differs for {expected['image']}")
    manifests = index["manifests"]
    runtimes = expected["platforms"]
    used = set()
    for arch, expected_digest in runtimes.items():
        runtime, = (item for item in manifests if item["digest"] == expected_digest
                    and item.get("platform", {}).get("architecture") == arch
                    and item["platform"].get("os") == "linux")
        require(runtime["platform"].get("variant") in (None, {"amd64": "v1", "arm64": "v8"}[arch]),
                f"unexpected platform variant for {expected['image']}/{arch}")
        attestations = [item for item in manifests
                        if item.get("annotations", {}).get("vnd.docker.reference.digest") == expected_digest]
        entries = [runtime, *attestations]
        require(all(item["mediaType"] == MANIFEST for item in entries),
                f"unexpected manifest type for {expected['image']}/{arch}")
        inspect_entries(layout, arch, entries, labels=labels)
        used.update(item["digest"] for item in entries)
    require(len(used) == len(manifests), f"unexpected or duplicate descriptor for {expected['image']}")


def check_version_tag(registry, ref, expected, *, bootstrap=False):
    raw = registry.manifest(ref, missing=True, bootstrap=bootstrap)
    if raw is None or raw == "bootstrap":
        return raw
    require(digest(raw) == expected, f"refusing to replace {ref}")
    return expected


def publish(registry, bundle, receipt, revision, version):
    check_bundle_checksums(bundle)
    entries = release_entries(json.loads((bundle / "release.json").read_bytes()),
                              revision, version, registry.namespace)
    labels = image_labels(revision, version)
    for role, entry in entries.items():
        check_layout(bundle / role, entry, labels)
    states = {}
    for role, entry in entries.items():
        states[role] = check_version_tag(registry, f"{entry['image']}:{version}", entry["index"], bootstrap=True)
    for role, entry in entries.items():
        if states[role] == "bootstrap":
            registry.copy(f"oci:{bundle / role}:release", f"docker://{entry['image']}@{entry['index']}")
            check_version_tag(registry, f"{entry['image']}:{version}", entry["index"])
    for role, entry in entries.items():
        ref = f"{entry['image']}:{version}"
        if check_version_tag(registry, ref, entry["index"]) is None:
            registry.copy(f"oci:{bundle / role}:release", f"docker://{ref}")
        registry.verify_digest(ref, entry["index"])
    write_json(receipt, {"revision": revision, "version": version, "images": entries})


def verify_anonymous(registry, entries, revision, version, directory):
    labels = image_labels(revision, version)
    directory.mkdir(parents=True, exist_ok=True)
    for role, entry in entries.items():
        ref = f"{entry['image']}:{version}"
        layout = directory / role
        registry.copy(f"docker://{ref}", f"oci:{layout}:release")
        check_layout(layout, entry, labels)
        print(f"Verified anonymous index {ref}@{entry['index']}", flush=True)


def promote_latest(registry, anonymous, entries, version):
    candidate = version_tuple(version)
    updates = {}
    higher = []
    for entry in entries.values():
        repository, expected = entry["image"], entry["index"]
        registry.verify_digest(f"{repository}:{version}", expected)
        alias = f"{repository}:latest"
        raw = registry.manifest(alias, missing=True)
        if raw is None:
            updates[alias] = f"{repository}@{expected}"
            continue
        index = json.loads(raw)
        require(index["mediaType"] == INDEX and index["annotations"]["org.opencontainers.image.source"] == SOURCE,
                f"unexpected OCI index for {alias}")
        current = version_tuple(index["annotations"]["org.opencontainers.image.version"])
        if current > candidate:
            higher.append(alias)
        elif current < candidate:
            updates[alias] = f"{repository}@{expected}"
        else:
            require(digest(raw) == expected, f"conflicting digest for {alias} at {version}")
    if higher:
        print(f"Skipped latest promotion for {version}. Higher versions exist at {', '.join(higher)}")
        return
    for alias, source in updates.items():
        registry.copy(f"docker://{source}", f"docker://{alias}")
    for entry in entries.values():
        alias = f"{entry['image']}:latest"
        anonymous.verify_digest(alias, entry["index"])
        print(f"Verified anonymous index {alias}@{entry['index']}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--loopback-registry")
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("publish", "verify-anonymous", "promote-latest"):
        action = commands.add_parser(command)
        action.add_argument("--receipt", required=True, type=Path)
        if command != "verify-anonymous":
            action.add_argument("--authfile", required=True, type=Path)
        if command == "publish":
            action.add_argument("--bundle", required=True, type=Path)
    args = parser.parse_args()
    require(SHA.fullmatch(args.revision), "invalid release revision")
    _, kind = release_tag(args.version, revision=args.revision)
    require(args.command != "promote-latest" or kind == "stable", "expected a stable OCI version")
    namespace = NAMESPACE
    if args.loopback_registry:
        require(re.fullmatch(r"127\.0\.0\.1:[0-9]{1,5}", args.loopback_registry), "expected a loopback registry")
        namespace = args.loopback_registry
    if args.command == "publish":
        publish(Registry(namespace, args.authfile), args.bundle, args.receipt, args.revision, args.version)
        return
    entries = release_entries(json.loads(args.receipt.read_bytes()), args.revision, args.version,
                              namespace, published=True)
    with tempfile.TemporaryDirectory(dir="/tmp", prefix="docsmcp-release-pull-") as temporary:
        directory = Path(temporary)
        authfile = directory / "auth.json"
        authfile.write_text("{}\n")
        anonymous = Registry(namespace, authfile, anonymous=True)
        if args.command == "verify-anonymous":
            verify_anonymous(anonymous, entries, args.revision, args.version, directory)
        else:
            promote_latest(Registry(namespace, args.authfile), anonymous, entries, args.version)


if __name__ == "__main__":
    try:
        main()
    except (KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
