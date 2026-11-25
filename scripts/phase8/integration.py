#!/usr/bin/env python3
"""Rehearse an independent restore behind the exact OxiBelt release edge."""

import argparse
from base64 import b64decode
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
from datetime import datetime, timezone
import fcntl
import http.client
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import ssl
import sys
import tempfile
import time

import backup
import edge
import images


ROOT = images.ROOT
sys.path.insert(0, str(ROOT / "scripts/phase5"))
spec = importlib.util.spec_from_file_location("phase5_integration", ROOT / "scripts/phase5/integration.py")
phase5 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase5)
phase3, phase2 = phase5.phase4.phase3, phase5.phase2
phase0 = phase2.phase0
BASE = ROOT / "compose.yml"
RELEASE = ROOT / "deploy/compose.release.yml"


@contextmanager
def release_stack(tag, refs):
    name = f"ghdocs8-{secrets.token_hex(5)}"
    table = name.replace("-", "_")
    root = Path(tempfile.mkdtemp(prefix=f"{name}-", dir="/tmp"))
    stack = phase0.Stack(root, name, tag)
    stack.mcp_host = "localhost"
    stack.compose_command[6:6] = ["-f", str(RELEASE)]
    stack.context.update({f"GH_DOCS_{role.upper()}_IMAGE": ref for role, ref in refs.items()})
    stack.context.update(GH_DOCS_MCP_AUTHORITY=f"localhost:{stack.env['PROXY_PORT']}",
                         EXPLORATION_CURSOR_KEY_ID="restore-v2", GH_DOCS_CONTROL_ENABLED="1")
    project = backup.Project(name, [BASE, RELEASE], stack.context)
    certificate = root / "proxy"
    os.chown(certificate, 10001, 10001)
    phase0.fixtures.cert(certificate, "localhost", "DNS:localhost", 10001)
    (certificate / "oxibelt.toml").write_text(edge.render(
        "localhost", stack.env["PROXY_IP"], stack.env["SERVE_INGRESS_IP"],
    ))
    for child in (root / "store").iterdir():
        if child.is_dir():
            shutil.rmtree(child)
        else:
            child.unlink()
    installed = False
    try:
        stack.create()
        phase0.firewall.install(table, stack.env, root, control=True)
        installed = True
        stack.one_shot("volume-admin")
        stack.compose("up", "--no-build", "--pull", "never", "--wait", "--wait-timeout", "130",
                      "postgres", timeout=145)
        yield stack, project, table
    except BaseException:
        print(stack.compose("logs", "--no-color", "--tail", "12", "proxy", "serve", "sync",
                            expected=None).stdout, flush=True)
        raise
    finally:
        stack.compose("down", "--volumes", "--remove-orphans", timeout=90)
        if installed:
            phase0.firewall.remove(table)
        shutil.rmtree(root)


def open_stack(stack):
    stack.compose("start", "serve", "sync", "feed", "sink", "proxy")
    for role in ("serve", "sync", "feed"):
        phase3.wait_until(lambda: stack.exec(role, "wget", "-qO", "/dev/null", "-T", "2",
                                            "http://127.0.0.1:8081/healthz",
                                            expected=None).returncode == 0, f"{role} did not start", seconds=25)
    def tls():
        try:
            with socket.create_connection(("127.0.0.1", int(stack.env["PROXY_PORT"])), timeout=2) as client:
                with ssl.create_default_context(cafile=stack.root / "ca.crt").wrap_socket(
                    client, server_hostname="localhost"
                ):
                    return True
        except OSError:
            return False
    phase3.wait_until(tls, "OxiBelt TLS did not start", seconds=25)
    pid = int(phase0.run([*phase0.DOCKER, "inspect", "--format", "{{.State.Pid}}",
                          stack.ids["proxy"]]).stdout)
    assert pid > 0
    operator_probe = ["nsenter", "--target", str(pid), "--net", "curl", "--noproxy", "*",
                      "--fail", "--silent", "--show-error", "--max-time", "5"]
    phase0.run([*operator_probe, "http://127.0.0.1:9091/ready"])
    metrics = phase0.run([*operator_probe, "http://127.0.0.1:9090/metrics"]).stdout
    assert "oxibelt_" in metrics


def proxy_probes(wire):
    authority = f"{wire.host}:{wire.port}"
    wire.send("tools/list", {}, changes={"Origin": "https://" + authority})
    wire.send("tools/list", {}, changes={
        "Forwarded": "host=other.invalid", "X-Forwarded-Host": "other.invalid",
    })
    # Incoming origin and authority remain independently validated.
    wire.send("tools/list", {}, status=403, changes={
        "Origin": "https://other.invalid", "X-Forwarded-Host": authority,
    })
    wire.send("tools/list", {}, status=403, changes={
        "Host": f"{wire.host}:1", "X-Forwarded-Host": authority,
    })
    for path in ("/", "/mcp/other", "/healthz", "/metrics", "/internal/v1/identify", "/admin/v1/status"):
        with closing(http.client.HTTPSConnection(wire.host, wire.port, context=wire.ssl, timeout=5)) as client:
            client.request("GET", path)
            response = client.getresponse()
            assert response.status == 404, (path, response.status)
            response.read()
    for context, name in ((ssl.create_default_context(cafile=wire.stack.root / "wrong-ca.crt"), "localhost"),
                          (wire.ssl, "other.invalid")):
        with socket.create_connection(("127.0.0.1", wire.port), timeout=5) as client:
            try:
                with context.wrap_socket(client, server_hostname=name):
                    raise AssertionError("proxy accepted an invalid TLS identity")
            except ssl.SSLError:
                pass
    print("A23 OxiBelt hostname, CA, authority, Origin, forwarding and private route checks passed", flush=True)


def choose_seed(stack, upstream, config, endpoint):
    wire = phase5.Wire(stack)
    wire.validate_registry()
    records = {}
    for fmt, (repo, _) in phase5.REPOS.items():
        retained = phase5.publish(stack, config, endpoint, fmt)
        context = wire.resolve(repo)
        read = {"context": context, "path": phase5.encoded(phase5.PATH), "encoding": "base64"}
        expected = wire.call("git.read", read)
        assert b64decode(expected["data"], validate=True) == phase5.NEW
        history = wire.call("git.log", {"context": context})
        source = phase5.repo_path(upstream, fmt)
        assert {entry["oid"] for entry in history["entries"]} == set(
            phase5.git(source, "rev-list", context["commit_oid"]).decode().splitlines())
        cursor_args = {"context": context, "pattern": phase5.PHRASE, "literal": True, "page": {"limit": 1}}
        token = wire.call("git.rg", cursor_args)["next_cursor"]
        assert token
        refs = phase5.git(source, "for-each-ref", "--format=%(refname)", "refs/heads/approved", "refs/tags")
        for ref in refs.decode().splitlines():
            phase5.git(source, "update-ref", "-d", ref)
        active, active_oid, _, active_raw = phase3.new_generation(
            stack, upstream, repo, endpoint, f"{fmt} forced replacement", fmt=fmt)
        phase5.phase4.prepare_publish(stack, repo, active, 1)
        wire.call("git.resolve", {"repository_id": repo, "selector": phase5.phase4.by_oid(context["commit_oid"])},
                  error="object_unavailable")
        ready, ready_oid, _, _ = phase3.new_generation(
            stack, upstream, repo, endpoint, f"{fmt} ready replacement", parent=active_oid, fmt=fmt)
        # Leave one real prepare interruption, then verify recovery before capture.
        if fmt == "sha1":
            phase3.command(stack, repo, "--prepare", ready, fault="after_refs")
            phase3.command(stack, repo, "--recover")
        else:
            phase3.command(stack, repo, "--prepare", ready)
        assert phase3.lifecycle(stack, repo, ready) == "ready"
        phase3.assert_pointer(stack, repo, active, 2)
        records[repo] = (retained, active, active_oid, active_raw, ready, ready_oid,
                         read, expected, history, cursor_args, token)
    return records


def check_readback(wire, records, *, rotate):
    for repo, (retained, active, active_oid, active_raw, ready, ready_oid,
               read, expected, history, cursor_args, token) in records.items():
        assert wire.resolve(repo)["commit_oid"] == active_oid
        current = wire.resolve(repo)
        value = wire.call("git.read", {"context": current, "path": phase5.encoded("original.bin"),
                                       "encoding": "base64"})
        assert b64decode(value["data"], validate=True) == active_raw
        assert wire.call("git.read", read) == expected
        assert wire.call("git.log", {"context": read["context"]}) == history
        assert wire.resolve(repo, generation=retained)["commit_oid"] == read["context"]["commit_oid"]
        wire.call("git.resolve", {"repository_id": repo, "generation_id": str(ready),
                                 "selector": phase5.phase4.by_oid(ready_oid)},
                  error="generation_unavailable")
        assert phase3.lifecycle(wire.stack, repo, active) == "active"
        assert phase3.lifecycle(wire.stack, repo, ready) == "ready"
        if rotate:
            wire.call("git.rg", {**cursor_args, "page": {"limit": 1, "cursor": token}}, error="cursor_invalid")
            fresh = wire.call("git.rg", cursor_args)["next_cursor"]
            assert fresh and fresh != token
            page = wire.call("git.rg", {**cursor_args, "page": {"limit": 1, "cursor": fresh}})
            assert page["scan_complete"]


def reject_damaged_backup(bundle, digest):
    pack = next((bundle / "store").rglob("*.pack"))
    other = pack.with_suffix(".omitted")
    pack.rename(other)
    try:
        try:
            backup.verify(bundle, digest)
        except ValueError:
            pass
        else:
            raise AssertionError("backup verification accepted a missing pack")
    finally:
        other.rename(pack)
    original = pack.read_bytes()
    try:
        pack.write_bytes(bytes([original[0] ^ 1]) + original[1:])
        try:
            backup.verify(bundle, digest)
        except ValueError:
            pass
        else:
            raise AssertionError("backup verification accepted same-length corruption")
    finally:
        pack.write_bytes(original)


def load(wire, records):
    repo = next(iter(records))
    context = records[repo][6]["context"]
    def observe(_):
        started = time.monotonic()
        result = wire.send("tools/call", {"name": "git.rg", "arguments": {
            "context": context, "pattern": phase5.PHRASE, "literal": True,
        }})["result"]
        return time.monotonic() - started, result["structuredContent"].get("code")
    cold = observe(0)
    assert cold[1] is None
    warm = [observe(i) for i in range(12)]
    assert all(result is None for _, result in warm)
    with ThreadPoolExecutor(max_workers=8) as pool:
        burst = list(pool.map(observe, range(8)))
    assert all(result in (None, "budget_exceeded") for _, result in burst)
    assert observe(0)[1] is None
    return {"first_seconds": cold[0], "warm_seconds": [elapsed for elapsed, _ in warm],
            "burst_seconds": [elapsed for elapsed, _ in burst],
            "budget_denials": sum(result == "budget_exceeded" for _, result in burst)}


def rehearse(args, refs, work):
    bundle = work / "backup"
    with phase2.stack(args.fixture_tag, publication=True, fixture_setup=phase5.prepare,
                      serve_target="mcp-serve") as (source, upstream, config, endpoint):
        records = choose_seed(source, upstream, config, endpoint)
        source.compose("stop", "proxy", "feed", "serve", "sync", "upstream")
        source_name = source.compose_command[source.compose_command.index("-p") + 1]
        project = backup.Project(source_name, [BASE, source.root / "phase2.json"], source.context)
        lock = source.root / "store/locks" / next(iter(records)) / "coordinator.lock"
        with lock.open("rb") as fence:
            fcntl.flock(fence, fcntl.LOCK_EX)
            try:
                backup.backup(project, source.root / "store", bundle)
            except BlockingIOError:
                assert not bundle.exists()
            else:
                raise AssertionError("backup accepted a busy filesystem fence")
        captured = backup.backup(project, source.root / "store", bundle)
    assert not source.root.exists()
    digest = captured["manifest_sha256"]
    reject_damaged_backup(bundle, digest)
    with release_stack(args.fixture_tag, refs) as (target, project, table):
        restored = backup.restore(project, project.store(), bundle, digest)
        # Alter the restored store only with the application containers still stopped.
        repo = next(iter(records))
        pack = next((target.root / "store/repositories" / repo).rglob("*.pack"))
        saved = pack.with_suffix(".omitted")
        pack.rename(saved)
        try:
            problem = target.compose("run", "--rm", "--no-deps", "-T", "sync", "--recover",
                                     "--repository", repo, expected=None, timeout=75)
            assert problem.returncode
            assert phase2.sql(target, "SELECT count(*) FROM git_catalog.publication_blocks") == "1"
        finally:
            saved.rename(pack)
        target.compose("run", "--rm", "--no-deps", "-T", "sync", "--recover", "--repository", repo, timeout=75)
        target.one_shot("migrate")
        target.one_shot("migrate")
        open_stack(target)
        wire = phase5.Wire(target)
        wire.validate_registry(host_rejection=(403, 404, 421))
        proxy_probes(wire)
        phase0.db_tests(target)
        phase0.network_tests(target, table)
        phase0.worker_tests(target)
        check_readback(wire, records, rotate=True)
        phase5.sdk(wire, args.node, args.client_dir, repo, records[repo][0])
        phase3.wait_until(lambda: phase2.sql(target, "SELECT count(*) FROM git_catalog.notification_outbox "
                                            "WHERE delivered_at IS NULL") == "0",
                          "restored outbox did not drain", seconds=40)
        measured = load(wire, records)
        for repo, record in records.items():
            phase3.command(target, repo, "--rollback", record[0], seq=2)
            phase3.command(target, repo, "--gc")
            assert wire.call("git.read", record[6]) == record[7]
            phase3.assert_pointer(target, repo, record[0], 3)
        target.compose("restart", "serve")
        phase3.wait_until(lambda: target.exec("serve", "wget", "-qO", "/dev/null", "-T", "2",
                                             "http://127.0.0.1:8081/healthz",
                                             expected=None).returncode == 0, "serve restart failed")
        for repo, record in records.items():
            assert wire.resolve(repo)["commit_oid"] == record[6]["context"]["commit_oid"]
    # Restore the same pre-upgrade state under the Phase 7 role binaries.
    previous = {role: f"gh-docs-mcp-{role}:{args.previous_tag}" for role in images.ROLES}
    with release_stack(args.fixture_tag, previous) as (target, project, _):
        try:
            backup.restore(project, project.store(), bundle, digest)
            open_stack(target)
            wire = phase5.Wire(target)
            wire.validate_registry(host_rejection=(403, 404, 421))
            check_readback(wire, records, rotate=True)
        finally:
            target.compose("stop", "proxy", "feed", "serve", "sync")
    return {"backup": captured, "restore": restored, "load": measured,
            "completed_utc": datetime.now(timezone.utc).isoformat()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--fixture-tag", required=True)
    parser.add_argument("--runtime-tag", required=True)
    parser.add_argument("--previous-tag", required=True)
    parser.add_argument("--node", required=True, type=Path)
    parser.add_argument("--client-dir", required=True, type=Path)
    args = parser.parse_args()
    if not __debug__ or os.getuid() != 0 or not args.evidence.resolve().is_relative_to("/tmp"):
        parser.error("run under sudo with assertions and evidence under /tmp")
    signal.signal(signal.SIGTERM, phase0.terminate)
    with (tempfile.TemporaryDirectory(dir="/tmp", prefix="ghdocs8-rehearsal-") as directory,
          images.load_native(args.bundle, args.runtime_tag) as refs):
        result = rehearse(args, refs, Path(directory))
    with args.evidence.open("x") as evidence:
        json.dump(result, evidence, indent=2)
        evidence.write("\n")
    print(json.dumps(result, sort_keys=True), flush=True)


if __name__ == "__main__":
    main()
