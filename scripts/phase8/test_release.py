"""Regression tests for release source checks and OCI publication."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import Mock, patch

import images
import publish
import release_source as source
import release_version as cargo_version


REVISION = "a" * 40
TAG_OBJECT = "b" * 40
VERSION = "0.9.0"


def workspace_file(name):
    return (cargo_version.ROOT / name).read_text()


class CargoVersionTests(unittest.TestCase):
    def test_stamp_changes_only_production_versions(self):
        committed = {name: tomllib.loads(workspace_file(name)) for name in cargo_version.CARGO_INPUTS}
        for version in ("1.0.2", "1.0.2-beta.1", cargo_version.CANDIDATE_VERSION):
            with self.subTest(version=version):
                stamped = cargo_version.stamped_inputs(workspace_file, version)
                expected = {name: tomllib.loads(workspace_file(name)) for name in stamped}
                expected["Cargo.toml"]["workspace"]["package"]["version"] = version
                for package in expected["Cargo.lock"]["package"]:
                    if package["name"] in cargo_version.PRODUCTION.values() and "source" not in package:
                        package["version"] = version
                self.assertEqual(set(stamped), set(committed))
                self.assertEqual({name: tomllib.loads(text) for name, text in stamped.items()}, expected)
                self.assertEqual({name: tomllib.loads(workspace_file(name)) for name in stamped}, committed)

    def test_workspace_rejects_bad_sentinel_metadata(self):
        for path, old, new, expected in (
            ("Cargo.toml", 'version = "0.0.0"', 'version = "1.0.2"', "workspace.package.version"),
            ("apps/Cargo.toml", "version.workspace = true", 'version = "0.0.0"', "apps/Cargo.toml"),
            ("crates/test-support/Cargo.toml", 'version = "0.0.0"', "version.workspace = true", "test-support"),
            ("Cargo.lock", 'name = "gh-docs-core"\nversion = "0.0.0"',
             'name = "gh-docs-core"\nversion = "1.0.2"', "Cargo.lock.*gh-docs-core"),
        ):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, expected):
                cargo_version.stamped_inputs(
                    lambda name: workspace_file(name).replace(old, new, 1) if name == path else workspace_file(name),
                    "1.0.2",
                )


class SourceTests(unittest.TestCase):
    def test_release_event_and_version(self):
        version = f"{VERSION}-beta.1"
        self.assertEqual(source.release_tag(version), (VERSION, "beta"))
        self.assertEqual(source.release_tag(VERSION), (VERSION, "stable"))
        for invalid in ("0.0.0", "v0.9.0", "0.09.0", "0.9.0-beta.0", "0.9.0-build.aaaaaaaa", "0.9.0-rc.1"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                source.release_tag(invalid)
        release = {"action": "published", "release": {"tag_name": version, "draft": False, "prerelease": True}}
        source.verify_event("release", release, version, "beta")
        for event, fields in (("push", {}), ("release", {"draft": True}), ("release", {"prerelease": False})):
            with self.subTest(event=event, fields=fields), self.assertRaises(ValueError):
                source.verify_event(event, {**release, "release": {**release["release"], **fields}}, version, "beta")

    def test_preflight_checks_exact_tag_and_only_then_stages(self):
        ref = f"refs/tags/{VERSION}"

        def git(*args):
            if args == ("rev-parse", "--verify", ref):
                return TAG_OBJECT
            if args == ("rev-parse", "--verify", f"{ref}^{{commit}}"):
                return REVISION
            if args == ("merge-base", "--is-ancestor", REVISION, "refs/remotes/origin/main"):
                return ""
            self.assertEqual(args[0], "show")
            self.assertTrue(args[1].startswith(f"{REVISION}:"))
            return workspace_file(args[1].split(":", 1)[1])

        staged = Mock()
        with (patch.object(source, "git", side_effect=git), patch.object(source, "verify_tag") as tag,
              patch.object(source, "verify_ci") as ci, patch.object(source, "stage_source", staged)):
            source.preflight(VERSION)
            tag.assert_called_once_with(VERSION, REVISION, TAG_OBJECT)
            ci.assert_called_once_with(REVISION)
            staged.assert_called_once()
            revision, version, directory = staged.call_args.args
            self.assertEqual((revision, version), (REVISION, VERSION))
            self.assertTrue(directory.is_relative_to("/tmp"))
            self.assertFalse(directory.exists())

            staged.reset_mock()
            ci.side_effect = ValueError("CI failed")
            with self.assertRaisesRegex(ValueError, "CI failed"):
                source.preflight(VERSION)
            staged.assert_not_called()

    def test_signed_annotated_tag(self):
        current = {"ref": f"refs/tags/{VERSION}", "object": {"type": "tag", "sha": TAG_OBJECT}}
        tag = {"tag": VERSION, "sha": TAG_OBJECT, "object": {"type": "commit", "sha": REVISION},
               "verification": {"verified": True}}
        with patch.object(source, "api", side_effect=[current, tag]):
            source.verify_tag(VERSION, REVISION, TAG_OBJECT)
        for invalid in ({"type": "commit", "sha": REVISION}, {"type": "tag", "sha": "c" * 40}):
            with self.subTest(invalid=invalid), patch.object(source, "api", return_value={**current, "object": invalid}):
                with self.assertRaisesRegex(ValueError, "remote annotated tag changed"):
                    source.verify_tag(VERSION, REVISION, TAG_OBJECT)
        with patch.object(source, "api", side_effect=[current, {**tag, "verification": {"verified": False}}]):
            with self.assertRaisesRegex(ValueError, "did not verify"):
                source.verify_tag(VERSION, REVISION, TAG_OBJECT)

    def test_canonical_ci(self):
        repo = {"full_name": source.REPOSITORY}
        success = {"status": "completed", "conclusion": "success"}
        run = {**success, "id": 8, "run_attempt": 2, "html_url": "run-url", "repository": repo,
               "head_repository": repo, "head_branch": "main", "head_sha": REVISION,
               "event": "push", "path": source.CI_PATH}
        job = {**success, "name": "rust", "head_sha": REVISION, "html_url": "job-url",
               "check_run_url": f"https://api.github.com/repos/{source.REPOSITORY}/check-runs/42"}
        check = {**success, "name": "rust", "head_sha": REVISION, "details_url": "job-url",
                 "app": {"id": 15368, "slug": "github-actions"}}
        runs = [{"workflow_runs": [run, {**run, "id": 9, "event": "pull_request"}]}]
        with patch.object(source, "api", side_effect=[runs, [{"jobs": [job]}], check, run]) as api:
            source.verify_ci(REVISION)
            api.assert_any_call("actions/runs/8/attempts/2/jobs?per_page=100", paginate=True)
        with patch.object(source, "api", return_value=[{"workflow_runs": [{**run, "head_sha": "c" * 40}]}]):
            with self.assertRaisesRegex(ValueError, "no canonical"):
                source.verify_ci(REVISION)
        with patch.object(source, "api", return_value=[{"workflow_runs": [run, {**run, "id": 9, "conclusion": "failure"}]}]):
            with self.assertRaisesRegex(ValueError, "newest canonical CI did not pass"):
                source.verify_ci(REVISION)
        with patch.object(source, "api", side_effect=[runs, [{"jobs": [job]}], check, {**run, "run_attempt": 3}]):
            with self.assertRaisesRegex(ValueError, "changed attempt"):
                source.verify_ci(REVISION)


def blob(layout, media_type, value):
    raw = json.dumps(value, sort_keys=True).encode()
    checksum = publish.digest(raw)
    images.blob_path(layout, checksum).write_bytes(raw)
    return {"mediaType": media_type, "digest": checksum, "size": len(raw)}


def bundle(directory, revision, version):
    directory.mkdir()
    config_type = "application/vnd.oci.image.config.v1+json"
    for arch in images.ARCHES:
        native = directory / arch
        labels = images.image_labels(revision, version)
        manifests = {}
        for role in images.ROLES:
            layout = native / role
            (layout / "blobs/sha256").mkdir(parents=True)
            config = blob(layout, config_type, {
                "architecture": arch, "os": "linux", "rootfs": {"type": "layers", "diff_ids": []},
                "config": {"Labels": {**labels, "test.role": role}},
            })
            runtime = blob(layout, publish.MANIFEST, {"schemaVersion": 2, "mediaType": publish.MANIFEST,
                                                      "config": config, "layers": []})
            runtime["platform"] = {"os": "linux", "architecture": arch}
            manifests[role] = runtime["digest"]
            layers = []
            for kind in ("https://spdx.dev/Document", "https://slsa.dev/provenance/v0.2"):
                layer = blob(layout, "application/vnd.in-toto+json", {
                    "predicateType": kind, "subject": [{"digest": {"sha256": runtime["digest"][7:]}}],
                })
                layer["annotations"] = {"in-toto.io/predicate-type": kind}
                layers.append(layer)
            attestation = blob(layout, publish.MANIFEST, {"schemaVersion": 2, "mediaType": publish.MANIFEST,
                                                          "config": blob(layout, config_type, {}), "layers": layers})
            attestation["platform"] = {"os": "unknown", "architecture": "unknown"}
            attestation["annotations"] = {"vnd.docker.reference.type": "attestation-manifest",
                                          "vnd.docker.reference.digest": runtime["digest"]}
            images.write_json(layout / "index.json", {"schemaVersion": 2, "manifests": [runtime, attestation]})
        gate = {"platform": arch, "revision": revision, "images": manifests}
        images.write_json(native / "receipt.json", {**gate, "version": version, "source": images.SOURCE,
                                                   "source_date_epoch": 1, "repeated": True,
                                                   "cargo_inputs": {name: "a" * 64 for name in cargo_version.CARGO_INPUTS}})
        images.write_json(native / "qualification.json", {**gate, "result": "pass"})
    assembled = directory / "bundle"
    images.assemble(directory / "amd64", directory / "arm64", assembled, revision=revision, version=version)
    seal(assembled)
    return assembled


def seal(assembled):
    files = sorted(path for path in assembled.rglob("*") if path.is_file() and path.name != "SHA256SUMS")
    (assembled / "SHA256SUMS").write_text("".join(
        f"{images.sha256(path)}  ./{path.relative_to(assembled)}\n" for path in files))


class BundleTests(unittest.TestCase):
    def test_assembly_requires_the_qualified_postgres_manifest(self):
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="ghdocs-release-test-") as temporary:
            work = Path(temporary) / "input"
            bundle(work, REVISION, VERSION)
            native = work / "arm64"
            build = json.loads((native / "receipt.json").read_bytes())
            gate = json.loads((native / "qualification.json").read_bytes())
            del gate["images"]["postgres"]
            images.write_json(native / "qualification.json", gate)
            with self.assertRaisesRegex(ValueError, "qualification receipt is missing"):
                images.assemble(work / "amd64", native, work / "missing-postgres")
            gate["images"]["postgres"] = build["images"]["postgres"] = f"sha256:{'d' * 64}"
            images.write_json(native / "qualification.json", gate)
            images.write_json(native / "receipt.json", build)
            with self.assertRaisesRegex(ValueError, "does not match the qualified manifest"):
                images.assemble(work / "amd64", native, work / "wrong-postgres")

    def test_assembly_rejects_different_stamped_inputs(self):
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="ghdocs-release-test-") as temporary:
            work = Path(temporary) / "input"
            bundle(work, REVISION, VERSION)
            path = work / "arm64" / "receipt.json"
            receipt = json.loads(path.read_bytes())
            receipt["cargo_inputs"]["Cargo.lock"] = "b" * 64
            images.write_json(path, receipt)
            with self.assertRaisesRegex(ValueError, "same reproducible release"):
                images.assemble(work / "amd64", work / "arm64", work / "new-bundle")

    def test_corrupt_bundle_blocks_all_registry_access(self):
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="ghdocs-release-test-") as temporary:
            work = Path(temporary)
            assembled = bundle(work / "valid", REVISION, VERSION)
            registry = Mock(namespace=publish.NAMESPACE)
            receipt = assembled / "release.json"
            data = json.loads(receipt.read_bytes())
            data["images"]["serve"]["index"] = f"sha256:{'d' * 64}"
            images.write_json(receipt, data)
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                publish.publish(registry, assembled, work / "receipt.json", REVISION, VERSION)
            seal(assembled)
            with self.assertRaisesRegex(ValueError, "layout index digest"):
                publish.publish(registry, assembled, work / "receipt.json", REVISION, VERSION)
            self.assertEqual(registry.mock_calls, [])


@unittest.skipUnless(os.environ.get("GH_DOCS_TEST_LOOPBACK") == "1", "requires Docker and Skopeo")
class RegistryTests(unittest.TestCase):
    def test_publication_recovery_and_monotonic_latest(self):
        with (tempfile.TemporaryDirectory(dir="/tmp", prefix="ghdocs-release-test-") as temporary,
              images.local_registry() as host):
            work = Path(temporary)
            auth = work / "auth.json"
            auth.write_text("{}\n")
            registry = publish.Registry(host, auth)
            anonymous = publish.Registry(host, auth, anonymous=True)
            receipt = work / "receipt.json"
            assembled = bundle(work / "stable", REVISION, VERSION)
            copy = registry.copy
            manifest = registry.manifest

            def interrupt(source, destination):
                if "-sync:" in destination:
                    raise subprocess.CalledProcessError(1, "simulated interrupted publication")
                copy(source, destination)

            def first_package(ref, *, missing=False, bootstrap=False):
                if bootstrap and ref == f"{host}/gh-docs-mcp-serve:{VERSION}":
                    return "bootstrap"
                return manifest(ref, missing=missing)

            with (patch.object(registry, "copy", side_effect=interrupt) as copied,
                  patch.object(registry, "manifest", side_effect=first_package)):
                with self.assertRaises(subprocess.CalledProcessError):
                    publish.publish(registry, assembled, receipt, REVISION, VERSION)
                serve = json.loads((assembled / "release.json").read_bytes())["images"]["serve"]["index"]
                self.assertEqual(copied.call_args_list[0].args[1], f"docker://{host}/gh-docs-mcp-serve@{serve}")
            self.assertFalse(receipt.exists())
            self.assertIsNotNone(registry.manifest(f"{host}/gh-docs-mcp-serve:{VERSION}"))
            self.assertIsNone(registry.manifest(f"{host}/gh-docs-mcp-sync:{VERSION}", missing=True))
            publish.publish(registry, assembled, receipt, REVISION, VERSION)
            stable = json.loads(receipt.read_bytes())["images"]
            self.assertEqual(set(stable), {"serve", "sync", "feed", "postgres"})
            with patch.object(registry, "copy", wraps=copy) as copied:
                publish.publish(registry, assembled, receipt, REVISION, VERSION)
                copied.assert_not_called()
            conflict = bundle(work / "conflict", TAG_OBJECT, VERSION)
            with self.assertRaisesRegex(ValueError, "refusing to replace"):
                publish.publish(registry, conflict, receipt, TAG_OBJECT, VERSION)
            subprocess.run([sys.executable, "-B", Path(publish.__file__), "--loopback-registry", host,
                            "--revision", REVISION, "--version", VERSION, "verify-anonymous",
                            "--receipt", receipt], check=True, timeout=60)
            publish.promote_latest(registry, anonymous, stable, VERSION)

            beta = "0.10.0-beta.1"
            publish.publish(registry, bundle(work / "beta", REVISION, beta), receipt, REVISION, beta)
            beta_entries = json.loads(receipt.read_bytes())["images"]
            publish.verify_anonymous(anonymous, beta_entries, REVISION, beta, work / "pulled-beta")
            with self.assertRaisesRegex(ValueError, "expected a stable OCI version"):
                publish.promote_latest(registry, anonymous, beta_entries, beta)

            newer = "0.10.0"
            publish.publish(registry, bundle(work / "newer", REVISION, newer), receipt, REVISION, newer)
            newer_entries = json.loads(receipt.read_bytes())["images"]
            publish.verify_anonymous(anonymous, newer_entries, REVISION, newer, work / "pulled-newer")
            with patch.object(registry, "copy", side_effect=interrupt):
                with self.assertRaises(subprocess.CalledProcessError):
                    publish.promote_latest(registry, anonymous, newer_entries, newer)
            with patch.object(registry, "copy", wraps=copy) as copied:
                publish.promote_latest(registry, anonymous, stable, VERSION)
                copied.assert_not_called()
            publish.promote_latest(registry, anonymous, newer_entries, newer)
            with patch.object(registry, "copy", wraps=copy) as copied:
                publish.promote_latest(registry, anonymous, stable, VERSION)
                copied.assert_not_called()
            for entry in newer_entries.values():
                anonymous.verify_digest(f"{entry['image']}:latest", entry["index"])
