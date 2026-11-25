"""Qualify production Git MCP boundaries using disposable synthetic data."""

import argparse
from contextlib import closing
from datetime import datetime, timezone
import importlib.util
import json
from pathlib import Path
import signal
import socket
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase5"))
spec = importlib.util.spec_from_file_location("phase5_integration", ROOT / "scripts/phase5/integration.py")
phase5 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase5)
phase3, phase2 = phase5.phase4.phase3, phase5.phase2

BLOB_SIZE = 5 * 1024 * 1024
BINARY_PATHS = 26


def prepare(root):
    phase5.prepare(root)
    origin = phase5.repo_path(root, "sha1")
    blob = phase5.git(origin, "hash-object", "-w", "--stdin", data=bytes(BLOB_SIZE)).strip()
    entries = b"".join(b"100644 blob " + blob + f"\t{number:03d}\n".encode()
                       for number in range(BINARY_PATHS))
    tree = phase5.git(origin, "mktree", data=entries).strip()
    oid = phase5.git(origin, "commit-tree", tree, "-m", "phase7 repeated binary").decode().strip()
    phase5.git(origin, "update-ref", "refs/heads/approved/phase7-binary", oid)
    oracle = root / "sha1-oracle.json"
    proof = json.loads(oracle.read_text())
    oracle.write_text(json.dumps({**proof, "binary_page": oid}))


def inflated_search_budget(wire, repo, proof):
    ctx = wire.resolve(repo, proof["binary_page"])
    args = {"context": ctx, "pattern": "absent", "literal": True}
    first = wire.call("git.rg", args)
    assert not first["scan_complete"] and not first["matches"] and first["next_cursor"]
    assert first["coverage"]["examined_bytes"] == 120 * 1024 * 1024, first
    second = wire.call("git.rg", {**args, "page": {"cursor": first["next_cursor"]}})
    assert second["scan_complete"] and not second["matches"] and second["next_cursor"] is None
    assert (first["coverage"]["examined_bytes"] + second["coverage"]["examined_bytes"]
            == BINARY_PATHS * BLOB_SIZE)
    print("A17/A20 inflated search byte budget returned a progressing cursor across 130 MiB", flush=True)


def malformed_candidates(wire, upstream, endpoint, fmt, repo, active, proof):
    stack = wire.stack
    target, oid, _, _ = phase3.new_generation(stack, upstream, repo, endpoint, "phase7 candidate",
                                              fmt=fmt, parent=proof["head"])
    job = phase2.sql(stack, f"SELECT job_id FROM git_catalog.private_candidates "
                            f"WHERE repository_id='{repo}' AND generation_id={target}")
    pack = next((stack.root / "staging/incoming" / job / "odb/objects/pack").glob("*.pack"))
    path = pack.with_suffix(".idx")
    original = path.read_bytes()
    try:
        path.write_bytes(original[:8])
        phase3.command(stack, repo, "--prepare", target, deny=True)
        phase3.assert_pointer(stack, repo, active, 1)
        assert wire.resolve(repo)["commit_oid"] == proof["head"]
    finally:
        path.write_bytes(original)

    phase3.command(stack, repo, "--prepare", target)
    wire.call("git.resolve", {"repository_id": repo, "selector": phase5.phase4.by_oid(oid)},
              error="object_unavailable")
    phase3.publish(stack, repo, target, 1)
    assert wire.resolve(repo)["commit_oid"] == oid
    assert wire.resolve(repo, generation=active)["commit_oid"] == proof["head"]
    result = phase3.command(stack, repo, "--rollback", active, seq=2)
    assert result["sequence"] == "3"
    assert wire.resolve(repo)["commit_oid"] == proof["head"]
    print(f"A08/A10/A12 {fmt} truncated index, ready visibility, retained read and rollback passed", flush=True)


def failed_acquisition(wire, upstream, endpoint, repo, expected):
    stack = wire.stack
    mode = upstream / "mode"
    origin = phase5.repo_path(upstream, "sha1")
    oid, _, _ = phase3.commit(origin, "phase7 transfer")
    phase5.git(origin, "update-ref", "refs/heads/main", oid)
    for name, reasons in (
        ("malformed", {"git_worker_failed", "invalid_git_protocol"}),
        ("partial", {"upstream_unavailable", "invalid_git_protocol", "git_worker_failed"}),
    ):
        try:
            mode.write_text(name)
            phase2.fetch(stack, repo, endpoint, reason=reasons)
        finally:
            mode.write_text("normal")
        assert wire.resolve(repo) == expected
    phase2.fetch(stack, repo, endpoint, reason={"storage_quota"},
                 env=("-e", "GH_DOCS_SYNC_QUOTA_BYTES=402653184"))
    assert wire.resolve(repo) == expected
    invalid_tree = phase5.git(origin, "hash-object", "--literally", "-w", "-t", "tree",
                              "--stdin", data=b"invalid synthetic tree").strip()
    invalid_commit = phase5.git(origin, "commit-tree", invalid_tree, "-m", "malformed tree fixture").strip()
    try:
        phase5.git(origin, "update-ref", "refs/heads/main", invalid_commit)
        phase2.fetch(stack, repo, endpoint, reason={
            "upstream_unavailable", "git_worker_failed", "candidate_invalid", "invalid_git_protocol",
        })
        assert wire.resolve(repo) == expected
    finally:
        phase5.git(origin, "update-ref", "refs/heads/main", oid)
    phase2.fetch(stack, repo, endpoint)
    print("A07/A08/A20 malformed tree and transfers, quota denial, and retry passed", flush=True)


def worker_shutdown(wire, repo, proof):
    stack = wire.stack
    context = wire.resolve(repo, proof["slow"])
    with closing(wire.start("tools/call", {
        "name": "git.rg", "arguments": {"context": context, "pattern": "z{512}"},
    })) as request:
        hold = """
        for _ in $(seq 1 100); do
          pid=$(pgrep -f '^/usr/local/bin/gh-docs-search-worker$' | head -n 1)
          if [ -n "$pid" ] && kill -STOP "$pid" 2>/dev/null; then exit 0; fi
          sleep .02
        done
        exit 1
        """
        try:
            stack.exec("serve", "sh", "-ec", hold)
            lock = f"/git-store/locks/{repo}/repository.lock"
            inode = stack.exec("sync", "stat", "-c", "%i", lock).stdout
            assert stack.exec("sync", "flock", "-n", "-x", lock, "true", expected=None).returncode == 1
            assert "deadline" in phase3.command(stack, repo, "--gc", deny=True)
            started = time.monotonic()
            stack.compose("stop", "--timeout", "8", "serve", timeout=20)
            elapsed = time.monotonic() - started
            stopped = phase2.phase0.run([
                *phase2.phase0.DOCKER, "inspect", "--format", "{{.State.ExitCode}} {{.State.OOMKilled}}",
                stack.ids["serve"],
            ]).stdout.strip()
            assert stopped == "0 false", stopped
            stack.exec("sync", "flock", "-n", "-x", lock, "true")
            assert stack.exec("sync", "stat", "-c", "%i", lock).stdout == inode
        finally:
            if request.sock is not None:
                try:
                    request.sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            stack.compose("start", "serve")
    phase3.wait_until(
        lambda: stack.exec("serve", "wget", "-T", "1", "-qO", "/dev/null",
                           "http://127.0.0.1:8081/healthz", expected=None, timeout=5).returncode == 0,
        "serve did not become ready after shutdown",
        seconds=20,
    )
    assert wire.resolve(repo)["repository_id"] == repo
    print(f"A04/A20 serve shutdown drained a stopped search worker in {elapsed:.3f}s and reopened admission",
          flush=True)


def ref_ceiling(wire, upstream, endpoint, repo):
    origin = phase5.repo_path(upstream, "sha1")
    refs = phase5.git(origin, "for-each-ref", "--format=%(refname)", "refs/heads/main",
                     "refs/heads/approved", "refs/tags/v1").splitlines()
    count = 1025 - len(refs)
    assert 0 < count <= 1024
    head = phase5.git(origin, "rev-parse", "refs/heads/main").decode().strip()
    names = [f"refs/heads/approved/phase7-{number:04d}" for number in range(count)]
    phase5.git(origin, "update-ref", "--stdin", data="".join(f"create {name} {head}\n" for name in names).encode())
    expected = wire.resolve(repo)
    phase2.fetch(wire.stack, repo, endpoint, reason={"ref_policy"})
    assert wire.resolve(repo) == expected
    phase5.git(origin, "update-ref", "-d", names[-1])
    report = phase2.fetch(wire.stack, repo, endpoint)
    assert report["outcome"] == "verified", report
    assert wire.resolve(repo) == expected
    print("A20 1,024 approved refs admitted and 1,025 denied with the active publication intact", flush=True)


def pack_ceiling(wire, upstream, endpoint, repo):
    origin = phase5.repo_path(upstream, "sha1")
    stack = wire.stack
    active = wire.resolve(repo)
    ref = "refs/heads/main"
    initial = phase5.git(origin, "rev-parse", ref).decode().strip()
    mib = 1024 * 1024
    try:
        oid, _, _ = phase3.commit(origin, "phase7 pack inside budget", parent=initial, size=7 * mib)
        phase5.git(origin, "update-ref", ref, oid)
        admitted = phase2.fetch(stack, repo, endpoint)
        pack = next((stack.root / "staging/incoming" / admitted["job"] / "odb/objects/pack").glob("*.pack"))
        size = pack.stat().st_size
        assert 7 * mib <= size <= 8 * mib, size

        oid, _, _ = phase3.commit(origin, "phase7 pack above budget", parent=initial, size=8 * mib)
        phase5.git(origin, "update-ref", ref, oid)
        phase2.fetch(stack, repo, endpoint, reason={"candidate_invalid"})
        assert wire.resolve(repo) == active
    finally:
        phase5.git(origin, "update-ref", ref, initial)
    print(f"A20 fixture pack admission accepted {size} bytes and denied above the 8 MiB source limit", flush=True)


def tree_ceiling(wire, upstream, endpoint, repo):
    origin = phase5.repo_path(upstream, "sha1")
    stack = wire.stack
    ref = "refs/heads/main"
    original = phase5.git(origin, "rev-parse", ref).decode().strip()
    blob = phase5.git(origin, "hash-object", "-w", "--stdin", data=b"reviewed synthetic leaf\n").strip()
    leaf = b"100644 blob " + blob + b"\tleaf\n"
    tree = phase5.git(origin, "mktree", data=leaf).strip()
    for _ in range(63):
        tree = phase5.git(origin, "mktree", data=b"040000 tree " + tree + b"\tnested\n").strip()
    tree = phase5.git(origin, "mktree", data=b"040000 tree " + tree + b"\tnested\n" + leaf).strip()
    commit = phase5.git(origin, "commit-tree", tree, "-m", "phase7 tree depth").decode().strip()
    active = wire.resolve(repo)
    sequence = int(phase3.command(stack, repo, "--status")["sequence"])
    try:
        phase5.git(origin, "update-ref", ref, commit)
        staged = int(phase2.fetch(stack, repo, endpoint)["generation"])
        phase5.phase4.prepare_publish(stack, repo, staged, sequence)
        ctx = wire.resolve(repo)
        wire.call("git.read", {"context": ctx, "path": phase5.encoded(b"nested/" * 64 + b"leaf")},
                  error="budget_exceeded")
        assert wire.call("git.read", {"context": ctx, "path": phase5.encoded(b"leaf")})["data"] == (
            "reviewed synthetic leaf\n")
        phase3.command(stack, repo, "--rollback", int(active["generation_id"]), seq=sequence + 1)
        assert wire.resolve(repo) == active
    finally:
        phase5.git(origin, "update-ref", ref, original)
    print("A17/A20 deep traversal stopped at the reader budget and the following approved read succeeded",
          flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled for qualification")
    signal.signal(signal.SIGTERM, phase2.phase0.terminate)
    print(f"{datetime.now(timezone.utc).isoformat()} Phase 7 Compose qualification", flush=True)
    with phase2.stack(args.image_tag, publication=True, fixture_setup=prepare,
                      serve_target="mcp-serve") as (stack, upstream, configuration, endpoint):
        wire = phase5.Wire(stack)
        wire.validate_registry()
        phase2.phase0.worker_tests(stack)
        phase2.assert_private(stack)
        for fmt, (repo, _) in phase5.REPOS.items():
            policy = phase5.policy(fmt)
            policy["limits"]["response_bytes"] = 16 * 1024 * 1024
            phase2.register(stack, configuration, policy)
            generation = int(phase2.fetch(stack, repo, endpoint)["generation"])
            phase5.phase4.prepare_publish(stack, repo, generation)
            proof = json.loads((upstream / f"{fmt}-oracle.json").read_text())
            ctx = wire.resolve(repo)
            assert ctx["commit_oid"] == proof["head"]
            phase5.search(wire, ctx)
            if fmt == "sha1":
                inflated_search_budget(wire, repo, proof)
                phase5.budgets(wire, repo, proof)
                worker_shutdown(wire, repo, proof)
            malformed_candidates(wire, upstream, endpoint, fmt, repo, generation, proof)
        assert phase2.sql(stack, "SELECT count(*) = count(DISTINCT advisory_key) "
                                 "FROM git_catalog.lock_keys") == "t"
        repo, _ = phase5.REPOS["sha1"]
        failed_acquisition(wire, upstream, endpoint, repo, wire.resolve(repo))
        pack_ceiling(wire, upstream, endpoint, repo)
        tree_ceiling(wire, upstream, endpoint, repo)
        ref_ceiling(wire, upstream, endpoint, repo)
    print("Phase 7 Compose qualification passed", flush=True)


if __name__ == "__main__":
    main()
