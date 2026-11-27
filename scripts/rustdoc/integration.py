"""Qualify synthetic hosted and Git Rustdoc through Sync and the normal MCP endpoint."""

import argparse
from gzip import GzipFile, compress
from hashlib import sha256
import importlib.util
import io
import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import tarfile
import tempfile
import time
from types import SimpleNamespace

from runner import rpc

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase5"))
spec = importlib.util.spec_from_file_location("phase5_integration", ROOT / "scripts/phase5/integration.py")
phase5 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase5)
sys.path.insert(0, str(ROOT / "scripts/phase8"))
spec = importlib.util.spec_from_file_location("phase8_integration", ROOT / "scripts/phase8/integration.py")
phase8 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase8)
from object_fixtures import dir_entry, file_entry, tree
from smart_fixtures import git, repo_path

phase2 = phase5.phase2
TARGET = "x86_64-unknown-linux-gnu"
GIT_SOURCE = "413c3a96-a781-4bf6-a96e-5f08d6967f2e"
HOSTED_SOURCE = "35cf5ef7-85b9-4ee0-b006-65ad6a3bc4fa"
HOSTED_URL = f"/crate/rustdoc-fixture/1.0.0/{TARGET}/json/61.gz"
REF = "refs/heads/approved/rustdoc"
DOC_PATH = "rustdoc/rustdoc_fixture/fn.open.md"
SOURCE_PATH = "src/lib.rs"
GIT_PATH = "docs/source.md"
TERM = "Rustdoc fixture needle"


def capsule(text):
    def item(i, name, kind, inner, markdown=None, source=SOURCE_PATH, *, visibility="public", crate=0):
        return {
            "id": i, "name": name, "crate_id": crate, "visibility": visibility, "docs": markdown,
            "span": {"filename": source, "begin": [1, 1], "end": [1, 1]},
            "attrs": [], "inner": {kind: inner},
        }

    return json.dumps({
        "format_version": 61, "root": 0, "target": {"triple": TARGET}, "includes_private": False,
        "index": {
            0: item(0, "rustdoc_fixture", "module", {"items": [11, 12, 13, 14, 15, 16]}, "Intro"),
            11: item(11, "open", "function", {}, text),
            12: item(12, "secret", "function", {}, "hidden by generated glob"),
            13: item(13, "foreign", "function", {}, "external crate", crate=1),
            14: item(14, "outside", "function", {}, "outside", "../outside.rs"),
            15: item(15, "other", "function", {}, "other source file", "src/other.rs"),
            16: item(16, "private", "function", {}, "private item", visibility="crate"),
        },
    }).encode()


def fixture(root):
    phase5.prepare(root, extra_tls_names=("docs.rs", "index.crates.io", "static.crates.io"))
    # Populate an exact synthetic registry record and archive. The Git lockfile pins its bytes.
    dependency = root / "fixture-dep.crate"
    with dependency.open("wb") as output, GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed, \
            tarfile.open(fileobj=compressed, mode="w") as archive:
        for name, data in (
            ("Cargo.toml", b'[package]\nname="rustdoc-fixture-dep"\nversion="1.0.0"\nedition="2024"\n'),
            ("src/lib.rs", b"pub struct Dependency;\n"),
        ):
            member = tarfile.TarInfo(f"rustdoc-fixture-dep-1.0.0/{name}")
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    checksum = sha256(dependency.read_bytes()).hexdigest()
    (root / "fixture-index").write_text(json.dumps({
        "name": "rustdoc-fixture-dep", "vers": "1.0.0", "deps": [], "features": {},
        "cksum": checksum, "yanked": False,
    }) + "\n")
    (root / "hosted.gz").write_bytes(compress(capsule(TERM + " hosted first\nsecond needle\n")))
    (root / "rustdoc-routes.json").write_text(json.dumps({
        HOSTED_URL: "hosted.gz",
        "/ru/st/rustdoc-fixture-dep": "fixture-index",
        "/crates/rustdoc-fixture-dep/rustdoc-fixture-dep-1.0.0.crate": "fixture-dep.crate",
    }))
    repo = repo_path(root, "sha256")
    lock = f"""version = 4

[[package]]
name = "rustdoc-fixture"
version = "1.0.0"
dependencies = ["rustdoc-fixture-dep"]

[[package]]
name = "rustdoc-fixture-dep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{checksum}"
"""
    manifest = b'[package]\nname="rustdoc-fixture"\nversion="1.0.0"\nedition="2024"\n[dependencies]\nrustdoc-fixture-dep="=1.0.0"\n'
    body = b'#![doc = "intro"]\n/// Rustdoc fixture needle\npub fn open() -> rustdoc_fixture_dep::Dependency { rustdoc_fixture_dep::Dependency }\n'
    workspace = [
        file_entry(repo, b"Cargo.toml", manifest),
        file_entry(repo, b"Cargo.lock", lock.encode()),
        file_entry(repo, b"build.rs", b"""
fn main() {
    assert_eq!(std::env::var("DOCS_RS").as_deref(), Ok("1"));
    assert!(std::env::var_os("DB_PASSWORD_FILE").is_none());
    assert!(!std::path::Path::new("/git-store").exists());
    assert!(!std::path::Path::new("/var/run/docker.sock").exists());
    assert_eq!(std::fs::read_to_string("/proc/net/route").unwrap().lines().count(), 1);
}
"""),
        dir_entry(repo, b"docs", [file_entry(repo, b"source.md", (TERM + " git file\n").encode())]),
        dir_entry(repo, b"src", [file_entry(repo, b"lib.rs", body)]),
    ]
    tip = git(repo, "commit-tree", tree(repo, workspace), "-m", "Synthetic Rustdoc crate").strip().decode()
    git(repo, "update-ref", REF, tip)
    (root / "rustdoc-tip").write_text(tip)


def run(*args):
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)


def certificates(stack):
    credentials = stack.root / "runner"
    credentials.mkdir(mode=0o700)
    ca = credentials / "ca.crt"
    ca_key = credentials / "ca.key"
    run("openssl", "req", "-new", "-x509", "-nodes", "-days", "1", "-newkey", "rsa:2048",
        "-subj", "/CN=Synthetic Rustdoc CA",
        "-addext", "basicConstraints=critical,CA:TRUE",
        "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        "-addext", "subjectKeyIdentifier=hash", "-keyout", ca_key, "-out", ca)
    for endpoint, usage in (("server", "serverAuth"), ("client", "clientAuth")):
        key, csr, cert, ext = [credentials / f"{endpoint}.{suffix}" for suffix in ("key", "csr", "crt", "ext")]
        run("openssl", "req", "-new", "-nodes", "-newkey", "rsa:2048", "-subj", "/CN=rustdoc.fixture",
            "-keyout", key, "-out", csr)
        ext.write_text(
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n"
            "subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n"
            f"subjectAltName=DNS:rustdoc.fixture\nextendedKeyUsage={usage}\n"
        )
        run("openssl", "x509", "-req", "-in", csr, "-CA", ca, "-CAkey", ca_key, "-CAcreateserial",
            "-out", cert, "-days", "1", "-extfile", ext)
    for name, source in (("rustdoc-server.crt", "server.crt"), ("rustdoc-server.key", "server.key"),
                         ("rustdoc-client-ca.crt", "ca.crt")):
        target = stack.root / "sync" / name
        target.write_bytes((credentials / source).read_bytes())
        os.chown(target, 10002, 10002)
        target.chmod(0o400)
    return credentials


def eventually(operation, *, seconds=40):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = operation()
        if value:
            return value
        time.sleep(.25)
    raise AssertionError("timed out awaiting the synthetic Rustdoc operation")


def register(stack, path, source_id, input_spec):
    path.write_text(json.dumps({
        "source_id": source_id, "policy_version": 1, "input": input_spec,
        "reviewer": "Fixture operator", "license_evidence": "Operator-owned fixture",
        "notice": "Reviewed fixture Rustdoc", "search": True, "grants": [
            {"source_path_glob": "src/lib.rs", "path_glob": "rustdoc/**/fn.open.md"}
        ],
    }))
    stack.compose("run", "--rm", "--no-deps", "migrate", "--register-markdown-source",
                  "--approval-file", phase2.APPROVAL)


def source(wire, source_id):
    return next((entry["source"] for entry in wire.call("markdown.sources", {})["entries"]
                 if entry.get("source", {}).get("source_id") == source_id), None)


def transcript(tag, builder_image, builder_json, backup_path):
    def configure(stack, override):
        settings = json.loads(override.read_text())
        settings["services"]["serve"]["image"] = f"docs-mcp-serve:{tag}"
        settings["services"]["sync"]["environment"].update({
            "DOCS_MCP_RUSTDOC_ENABLED": "1",
            "DOCS_MCP_RUSTDOC_RUNNER_BIND": f"{stack.env['SYNC_CONTROL_IP']}:8785",
        })
        override.write_text(json.dumps(settings))
        certificates(stack)

    with phase2.stack(tag, control=True, fixture_setup=fixture, rustdoc_runner=True,
                      configure_stack=configure) as (stack, upstream, approval_file, endpoint):
        # Verify schema 8 migration with already published Git data.
        fmt = "sha256"
        repo, _ = phase5.REPOS[fmt]
        policy = phase5.glob_policy(fmt)
        phase2.register(stack, approval_file, policy)
        generation = phase5.publish_registered(stack, endpoint, fmt, 0)
        wire = phase5.Wire(stack)
        wire.validate_registry()
        tip = (upstream / "rustdoc-tip").read_text()
        context = wire.resolve(repo, tip, generation)
        stack.compose("stop", "sync")
        phase2.restore_v8(stack)
        stack.one_shot("migrate")
        stack.one_shot("migrate")
        stack.compose("start", "sync")
        eventually(lambda: stack.exec("sync", "wget", "-T", "2", "-qO", "/dev/null",
                                      "http://127.0.0.1:8081/healthz", expected=None).returncode == 0)
        assert wire.resolve(repo, tip, generation) == context
        assert phase2.sql(stack, "SELECT max(version) FROM git_catalog.migrations") == "9"
        assert phase2.sql(stack, """
            SELECT has_table_privilege('gh_docs_serve','git_catalog.markdown_documents','SELECT'),
                has_table_privilege('gh_docs_serve','git_catalog.readable_markdown_documents','UPDATE'),
                has_table_privilege('gh_docs_sync','git_catalog.markdown_approvals','INSERT')
        """) == "f|f|f"

        register(stack, approval_file, HOSTED_SOURCE, {
            "kind": "docsrs_rustdoc", "package": "rustdoc-fixture", "version": "1.0.0",
            "target": TARGET, "format": 61,
        })
        register(stack, approval_file, GIT_SOURCE, {
            "kind": "git_rustdoc", "repository_id": repo, "reference": REF,
            "package": "rustdoc-fixture", "target": TARGET,
        })
        credentials = stack.root / "runner"
        config = SimpleNamespace(host=stack.env["SYNC_CONTROL_IP"], port=8785, server_name="rustdoc.fixture")
        tls = ssl.create_default_context(cafile=credentials / "ca.crt")
        try:
            rpc(config, tls, {"operation": "lease"})
        except (ssl.SSLError, EOFError, ConnectionResetError):
            pass
        else:
            raise AssertionError("runner accepted an unauthenticated client")
        tls.load_cert_chain(credentials / "client.crt", credentials / "client.key")

        def lease(bundle):
            result = rpc(config, tls, {"operation": "lease"}, incoming=bundle)
            return result if result["status"] == "job" else None

        hosted = eventually(lambda: source(wire, HOSTED_SOURCE))
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="docs-mcp-rustdoc-probe-") as tmp:
            directory = Path(tmp)
            bundle = directory / "workspace.tar"
            job = eventually(lambda: lease(bundle))
            with tarfile.open(bundle) as archive:
                assert archive.extractfile("workspace/src/lib.rs").read().startswith(b"#![doc")
                assert archive.extractfile("registry/rustdoc-fixture-dep-1.0.0.crate").read() == (
                    upstream / "fixture-dep.crate").read_bytes()
            artifact = directory / "fixture.json"
            if builder_image:
                from runner import build
                config.image, config.qemu = builder_image, "/usr/bin/qemu-system-x86_64"
                assert build(config, directory, job, bundle, artifact)
            elif builder_json:
                artifact.write_bytes(builder_json.read_bytes())
            else:
                artifact.write_bytes(capsule(TERM + " built\n"))
            command = {"operation": "complete", "job_id": job["job_id"], "attempt": job["attempt"],
                       "body_bytes": artifact.stat().st_size}
            assert rpc(config, tls, {"operation": "fail", "job_id": job["job_id"],
                                     "attempt": job["attempt"]})["status"] == "accepted"
            phase2.sql(stack, f"UPDATE git_catalog.markdown_jobs SET due = clock_timestamp() "
                              f"WHERE job_id = '{job['job_id']}' AND state = 'queued'")
            retry = eventually(lambda: lease(directory / "retry.tar"))
            assert retry["job_id"] == job["job_id"] and retry["attempt"] == job["attempt"] + 1
            assert rpc(config, tls, command)["status"] == "rejected"
            command["attempt"] = retry["attempt"]
            assert rpc(config, tls, command, outgoing=artifact)["status"] == "accepted"
            assert rpc(config, tls, command)["status"] == "rejected"

        local = eventually(lambda: source(wire, GIT_SOURCE))
        assert local["context"] == context
        flat = {"kind": "git_markdown", "context": context}
        found = wire.call("markdown.find", {"sources": [local, flat, hosted]})
        assert {entry["document"]["path"]["value"] for entry in found["entries"]} == {DOC_PATH, GIT_PATH}
        search = {"sources": [local, flat, hosted], "pattern": TERM, "literal": True, "page": {"limit": 1}}
        first = wire.call("markdown.search", search)
        assert first["next_cursor"]
        wire.call("markdown.search", {**search, "path_glob": phase5.encoded("rustdoc/**"),
                  "page": {"limit": 1, "cursor": first["next_cursor"]}}, error="cursor_invalid")
        hits = []
        while True:
            page = wire.call("markdown.search", search)
            hits.extend(page["matches"])
            if not page["next_cursor"]:
                break
            search["page"]["cursor"] = page["next_cursor"]
        assert len(hits) == 3
        both = {"sources": [local, flat, hosted], "source_path_glob": phase5.encoded("src/**/*.rs"),
                "path_glob": phase5.encoded("rustdoc/**/fn.open.md")}
        assert len(wire.call("markdown.find", both)["entries"]) == 2
        assert not wire.call("markdown.find", {**both, "source_path_glob": phase5.encoded("docs/**")})["entries"]
        read = {"document": {"source": hosted, "path": phase5.encoded(DOC_PATH)}}
        assert wire.call("markdown.read", read)["data"].startswith(TERM + " hosted first")
        assert wire.call("markdown.read", {**read, "range": {"start": 0, "end": len(TERM)}})["data"] == TERM

        # Refresh the exact release, preserve the old locator, and reject a revoked locator.
        (upstream / "hosted.gz").write_bytes(compress(capsule(TERM + " hosted rebuilt\n")))
        stack.exec("sync", "docs-mcp-sync", "--markdown-retry", "--source", HOSTED_SOURCE)

        def refreshed():
            current = source(wire, HOSTED_SOURCE)
            return current if current != hosted else None

        fresh = eventually(refreshed)
        assert wire.call("markdown.read", {"document": {**read["document"], "source": fresh}})["data"].endswith("rebuilt\n")
        assert "first" in wire.call("markdown.read", read)["data"]
        stack.exec("sync", "docs-mcp-sync", "--markdown-retry", "--source", HOSTED_SOURCE)
        eventually(lambda: phase2.sql(stack, f"""
            SELECT count(*) FROM git_catalog.markdown_jobs
             WHERE source_id = '{HOSTED_SOURCE}' AND state = 'succeeded'
        """) == "3")
        assert source(wire, HOSTED_SOURCE) == fresh
        hidden_change = capsule(TERM + " hosted rebuilt\n").replace(
            b"hidden by generated glob", b"changed outside the output grant")
        (upstream / "hosted.gz").write_bytes(compress(hidden_change))
        stack.exec("sync", "docs-mcp-sync", "--markdown-retry", "--source", HOSTED_SOURCE)
        eventually(lambda: phase2.sql(stack, f"""
            SELECT count(*) FROM git_catalog.markdown_jobs
             WHERE source_id = '{HOSTED_SOURCE}' AND state = 'succeeded'
        """) == "4")
        assert source(wire, HOSTED_SOURCE) == fresh
        (upstream / "hosted.gz").write_bytes(b"invalid gzip")
        stack.exec("sync", "docs-mcp-sync", "--markdown-retry", "--source", HOSTED_SOURCE)
        eventually(lambda: "invalid_docsrs_json" in stack.exec(
            "sync", "docs-mcp-sync", "--markdown-status", "--source", HOSTED_SOURCE).stdout)
        assert source(wire, HOSTED_SOURCE) == fresh
        stack.compose("run", "--rm", "--no-deps", "migrate", "--revoke-markdown-source", "--source", HOSTED_SOURCE)
        wire.call("markdown.read", read, error="object_unavailable")
        assert source(wire, HOSTED_SOURCE) is None
        print("Synthetic Rustdoc scheduling, TLS, publication, dual globs, search, refresh and revocation passed", flush=True)
        doc = {"document": {"source": local, "path": phase5.encoded(DOC_PATH)}}
        expected = wire.call("markdown.read", doc)
        stack.compose("stop", "proxy", "feed", "sync", "serve")
        project_name = stack.compose_command[stack.compose_command.index("-p") + 1]
        project = phase8.backup.Project(project_name, [ROOT / "compose.yml", stack.root / "phase2.json"], stack.context)
        backup = phase8.backup.backup(project, project.store(), backup_path)
        digest = backup["manifest_sha256"]
        record = phase8.backup.verify(backup_path, digest)["catalog"]
        assert record["schema"] == 9 and any(snapshot["source_id"] == GIT_SOURCE
                                             for snapshot in record["markdown_snapshots"])
    return digest, doc, expected


def restore(tag, backup_path, digest, request, expected):
    refs = {role: f"docs-mcp-{role}:{tag}" for role in ("serve", "sync", "feed", "postgres")}
    with phase8.release_stack(tag, refs) as (stack, project, _):
        phase8.backup.restore(project, project.store(), backup_path, digest)
        phase8.open_stack(stack)
        wire = phase5.Wire(stack)
        wire.validate_registry(host_rejection=(403, 404, 421))
        assert wire.call("markdown.read", request) == expected
        assert source(wire, HOSTED_SOURCE) is None
    print("Independent schema 9 restore verified the Rustdoc snapshot and revocation", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    builder = parser.add_mutually_exclusive_group()
    builder.add_argument("--builder-image", type=Path)
    builder.add_argument("--builder-json", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(dir="/tmp", prefix="docs-mcp-rustdoc-restore-") as tmp:
        backup = Path(tmp) / "backup"
        result = transcript(args.image_tag, args.builder_image, args.builder_json, backup)
        restore(args.image_tag, backup, *result)
