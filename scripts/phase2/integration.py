"""Qualify bounded private synchronization in the rootful Compose profile."""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
from functools import partial
import ipaddress
import json
import os
import re
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile
import time

from smart_fixtures import HOST, REPOS, approval, commit, git, prepare, repo_path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase0"))
import firewall
import integration as phase0

CA = "/phase2-ca.crt"
APPROVAL = "/phase2-approval.json"


@contextmanager
def stack(tag, *, publication=False, fixture_setup=prepare, serve_target=None, control=False, host=HOST,
          configure_stack=None):
    phase = 6 if control else 5 if serve_target == "serve" else 4 if serve_target == "phase4-serve" else 3 if publication else 2
    project = f"docsmcp{phase}-{secrets.token_hex(5)}"
    root = Path(tempfile.mkdtemp(prefix=f"{project}-", dir="/tmp"))
    table = project.replace("-", "_")
    instance = phase0.Stack(root, project, tag)
    upstream = root / "upstream"
    fixture_setup(upstream)
    (upstream / "tls/upstream.key").chmod(0o640)
    os.chown(upstream / "tls/upstream.key", 0, 10002)
    approval_path = root / "approval.json"
    approval_path.touch()
    address = str(ipaddress.ip_network(instance.env["SYNC_CIDR"])[4])
    instance.env["UPSTREAM_IP"] = address
    override = root / "phase2.json"
    sync = "phase6-sync" if control else "phase3-sync" if publication else "phase2-sync"
    serve_target = serve_target or ("phase3-serve" if publication else None)
    serve = {"serve": {"image": f"docs-mcp-{serve_target}:{tag}"}} if serve_target else {}
    services = {
        **serve,
        "sync": {
            "image": f"docs-mcp-{sync}:{tag}",
            "extra_hosts": {host: address},
            "volumes": [{"type": "bind", "source": str(upstream / "tls/ca.crt"), "target": CA, "read_only": True}],
        },
        "migrate": {"volumes": [{"type": "bind", "source": str(approval_path), "target": APPROVAL, "read_only": True}]},
        "upstream": {
            "image": f"docs-mcp-phase2-upstream:{tag}", "user": "10002:10002",
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
            "read_only": True, "pids_limit": 64, "mem_limit": "256m",
            "networks": {"sync_egress": {"ipv4_address": address}},
            "volumes": [{"type": "bind", "source": str(upstream), "target": "/fixture", "read_only": True}],
        },
    }
    if control:
        configure_control(instance, services, tag, upstream, address)
    override.write_text(json.dumps({"services": services}))
    instance.compose_command[6:6] = ["-f", str(override)]
    installed = False
    cleaned = False
    try:
        if configure_stack:
            configure_stack(instance, override)
        instance.create()
        instance.compose("create", "--no-build", "--pull", "never", "upstream")
        firewall.install(table, instance.env, root, sync_fixture=True, control=control, feed_fixture=control)
        installed = True
        instance.start()
        instance.compose("start", "upstream")
        endpoint = f"{address}:8443"
        for _ in range(8):
            response = instance.exec("sync", "wget", "-T", "2", "-qO", "/dev/null", f"http://{address}:8088/health", expected=None, timeout=5)
            if response.returncode == 0:
                break
            time.sleep(0.25)
        else:
            print(instance.compose("ps", "--all", "upstream", expected=None).stdout, flush=True)
            print(instance.compose("logs", "--no-color", "upstream", expected=None).stdout, flush=True)
            raise RuntimeError(f"synthetic upstream did not become ready: {response.stdout}")
        yield instance, upstream, approval_path, endpoint
    except BaseException:
        roles = ("sync", "serve", "feed", "postgres", "upstream") if control else ("upstream",)
        print(instance.compose("logs", "--no-color", "--tail", "12", *roles, expected=None).stdout, flush=True)
        raise
    finally:
        try:
            instance.compose("down", "--volumes", "--remove-orphans", timeout=90)
            if installed:
                firewall.remove(table)
            cleaned = True
        finally:
            if cleaned:
                shutil.rmtree(root)
            else:
                print(f"cleanup pending for {project} at {root}", file=sys.stderr)


def configure_control(stack, services, tag, upstream, address):
    feed_address = str(ipaddress.ip_network(stack.env["FEED_CIDR"])[4])
    stack.env["FEED_UPSTREAM_IP"] = feed_address
    services["upstream"]["networks"]["feed_egress"] = {"ipv4_address": feed_address}
    for role in ("sync", "feed", "serve"):
        directory = stack.root / role
        (directory / "fault").touch()
        (directory / "roster.json").write_text("[]")
        config = services.setdefault(role, {})
        config["image"] = f"docs-mcp-phase6-{role}:{tag}"
        config["environment"] = {
            "DOCS_MCP_CONTROL_ENABLED": "1",
            "DOCS_MCP_PHASE6_FAULT_FILE": "/run/secrets/fault",
        }
        if role in ("sync", "feed"):
            host = address if role == "sync" else feed_address
            config["environment"].update({
                f"DOCS_MCP_{role.upper()}_ROSTER_FILE": "/run/secrets/roster.json",
                "DOCS_MCP_PHASE6_UPSTREAM": f"{host}:8443",
                "DOCS_MCP_PHASE6_UPSTREAM_CA": CA,
            })
            if role == "feed":
                config["volumes"] = [{"type": "bind", "source": str(upstream / "tls/ca.crt"),
                                       "target": CA, "read_only": True}]


def restore_v7(stack):
    """Rewind glob approval DDL in a disposable catalog with only legacy policies."""
    assert sql(stack, "SELECT count(*) FROM git_catalog.source_approvals "
                      "WHERE approval->'exploration'->'version' = '3'::jsonb") == "0"
    previous = (ROOT / "migrations/0006_output_approval.sql").read_text().split("INSERT INTO")[0]
    sql(stack, f"BEGIN; {previous} DELETE FROM git_catalog.migrations WHERE version = 8; COMMIT;")


def restore_v6(stack):
    """Rewind the Phase 6 DDL in a disposable probe while keeping old data."""
    restore_v7(stack)
    control = (ROOT / "migrations/0007_control_plane.sql").read_text()
    names = re.findall(r"^CREATE FUNCTION git_catalog\.(\w+)\(", control, re.MULTILINE)
    drops = "; ".join(f"DROP FUNCTION git_catalog.{name}" for name in names)
    previous = (ROOT / "migrations/0004_publication.sql").read_text()
    promote = previous[previous.index("CREATE FUNCTION git_catalog.promote_generation"):
                       previous.index("REVOKE EXECUTE ON ALL FUNCTIONS")]
    promote = promote.replace("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1)
    sql(stack, f"""BEGIN; {drops};
        DROP TABLE git_catalog.hint_receipts, git_catalog.scheduled_jobs, git_catalog.sync_schedule;
        ALTER TABLE git_catalog.sync_jobs DROP COLUMN scheduler_retry_after;
        ALTER TABLE git_catalog.notification_outbox DROP COLUMN delivery_attempts, DROP COLUMN next_attempt_at;
        {promote}
        DELETE FROM git_catalog.migrations WHERE version = 7; COMMIT;""")


def sql(stack, statement):
    return stack.exec("postgres", "psql", "-XAt", "-v", "ON_ERROR_STOP=1", "-U", "postgres",
                      "-d", "gh_docs", "-c", statement).stdout.strip()


def register(stack, path, policy, *, expected=0):
    path.write_text(json.dumps(policy))
    return stack.compose("run", "--rm", "--no-deps", "migrate", "--register-source", "--approval-file", APPROVAL,
                         expected=expected)


def fetch(stack, repository, endpoint, ca=CA, *, reason=None, env=(), binary="docs-mcp-phase2-fixture"):
    debug = ("-e", "DOCS_MCP_PHASE2_DEBUG=1") if os.environ.get("DOCS_MCP_PHASE2_DEBUG") == "1" else ()
    command = ["exec", "-T", *debug, *env, "sync", binary, repository, endpoint, ca]
    response = stack.compose(*command, expected=0 if reason is None else None, timeout=100)
    reports = [json.loads(line) for line in response.stdout.splitlines() if line.startswith("{")]
    assert len(reports) == 1, response.stdout
    report = reports[0]
    if reason:
        assert response.returncode and report["reason"] in reason, report
    print(json.dumps(report), flush=True)
    return report


def verify_candidate(stack, upstream, fmt, report):
    repository = REPOS[fmt][0]
    job = report["job"]
    target = stack.root / "staging/incoming" / job
    manifest = json.loads((target / "manifest.json").read_text())
    assert manifest[2][:3] == [repository, REPOS[fmt][1], fmt], manifest
    oracle = json.loads((upstream / f"{fmt}-oracle.json").read_text())
    def object_result(oid):
        return phase0.run(["git", "-c", "safe.directory=*", "--git-dir", target / "odb",
                           "cat-file", "-t", oid], expected=None)
    assert object_result(oracle["hidden"]).returncode != 0
    assert object_result(oracle["blob"]).stdout.strip() == "blob"
    raw = subprocess.check_output(["git", "-c", "safe.directory=*", "--git-dir", target / "odb",
                                   "cat-file", "blob", oracle["blob"]])
    assert raw == (upstream / f"{fmt}-oracle.bin").read_bytes()
    assert sql(stack, f"SELECT state FROM git_catalog.private_candidates WHERE job_id='{job}'") == "usable"
    return target, oracle


def assert_private(stack):
    assert sql(stack, "SELECT count(*) FROM git_catalog.visible_repositories") == "0"
    assert sql(stack, "SELECT count(*) FROM git_catalog.active_generations") == "0"
    assert sql(stack, "SELECT count(*) FROM git_catalog.notification_outbox") == "0"
    assert sql(stack, """
        SELECT has_table_privilege('gh_docs_serve', 'git_catalog.private_candidates', 'SELECT'),
            has_table_privilege('gh_docs_sync', 'git_catalog.private_candidates', 'UPDATE'),
            has_function_privilege('gh_docs_sync', 'git_catalog.revoke_source(uuid)', 'EXECUTE'),
            has_table_privilege('gh_docs_sync', 'git_catalog.active_generations', 'UPDATE')
    """) == "f|f|f|f"
    stack.exec("serve", "sh", "-ec", "test ! -e /git-sync-private")
    assert set(stack.exec("serve", "ls", "-A", "/git-store/repositories").stdout.splitlines()) == {
        "selected.git", "unrelated.git", "sha256.git",
    }


def interruption(stack, repository, endpoint, control, point):
    result = stack.compose("exec", "-T", "-e", f"DOCS_MCP_FIXTURE_FAULT={point}", "sync",
                           "docs-mcp-phase2-fixture", repository, endpoint, CA, expected=None, timeout=100)
    assert result.returncode and "fixture reconciliation failed" in result.stdout, result.stdout
    last = sql(stack, f"""SELECT j.job_id, j.state, coalesce(p.state,'')
        FROM git_catalog.sync_jobs j LEFT JOIN git_catalog.private_candidates p USING(job_id)
        WHERE j.repository_id='{repository}' ORDER BY created_at DESC LIMIT 1""").split("|")
    job, state, disposition = last
    artifact = stack.root / "staging/incoming" / job
    assert artifact.is_dir()
    if point == "sealed":
        assert (state, disposition) == ("running", ""), last
        fetch(stack, repository, endpoint)
        assert not artifact.exists() and (stack.root / "staging/quarantine" / job).is_dir()
        assert sql(stack, f"SELECT outcome FROM git_catalog.sync_jobs WHERE job_id='{job}'") == "interrupted"
    else:
        assert (state, disposition) == ("succeeded", "usable"), last
        retired = sql(stack, f"""SELECT p.job_id FROM git_catalog.private_candidates p
            JOIN git_catalog.sync_jobs j USING(job_id)
            WHERE p.repository_id='{repository}' AND p.state='retired' AND j.cleaned_at IS NULL""")
        assert retired and (stack.root / "staging/incoming" / retired).is_dir()
        assert fetch(stack, repository, endpoint)["outcome"] == "no_change"
        assert artifact.is_dir() and not (stack.root / "staging/incoming" / retired).exists()
    control.write_text("normal")
    assert_private(stack)


def concurrent_admission(stack, repository, endpoint, upstream):
    other = REPOS["sha256"][0]
    # Both jobs need fresh objects to enter the delayed fetch path.
    for fmt in ("sha1", "sha256"):
        origin = repo_path(upstream, fmt)
        git(origin, "update-ref", "refs/heads/main", commit(origin, f"{fmt} delayed update")[0])
    (upstream / "mode").write_text("delay")
    def command(repo):
        return [*stack.compose_command, "exec", "-T", "sync", "docs-mcp-phase2-fixture", repo, endpoint, CA]
    processes = [
        subprocess.Popen(command(repo), env=stack.context, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        for repo in (repository, other)
    ]
    try:
        for _ in range(40):
            if sql(stack, "SELECT count(*) FROM git_catalog.sync_jobs WHERE state='running'") == "2":
                break
            time.sleep(0.2)
        else:
            raise RuntimeError("concurrent jobs did not occupy both slots")
        # Check global admission before the repository fence's timed wait.
        third = stack.exec("sync", "docs-mcp-phase2-fixture", "11111111-2222-4333-8444-000000000003", endpoint, CA,
                           expected=None)
        assert third.returncode and '"reason":"global_limit"' in third.stdout, third.stdout
        same = stack.exec("sync", "docs-mcp-phase2-fixture", repository, endpoint, CA, expected=None)
        assert same.returncode and '"reason":"repository_writer"' in same.stdout, same.stdout
        # Revoke an admitted job while the synthetic endpoint is slow.
        stack.compose("run", "--rm", "--no-deps", "migrate", "--revoke-source", "--source", REPOS["sha256"][1])
        stack.exec("sync", "sh", "-ec",
                   "for proc in /proc/[0-9]*/cmdline; do "
                   "if tr '\\000' ' ' <\"$proc\" 2>/dev/null | grep -Eq \"^(/usr/local/bin/)?docs-mcp-phase2-fixture $1 \"; "
                   "then pid=${proc#/proc/}; kill -TERM \"${pid%/cmdline}\"; fi; done",
                   "cancel-fixture", repository)
        (upstream / "mode").write_text("normal")
        statuses = [process.communicate(timeout=45)[0] for process in processes]
        assert all(process.returncode for process in processes), statuses
        assert '"reason":"cancelled"' in statuses[0], statuses
        assert '"reason":"approval_unavailable"' in statuses[1], statuses
    finally:
        (upstream / "mode").write_text("normal")
        for process in processes:
            if process.poll() is None:
                process.terminate()
                process.communicate(timeout=10)
    assert_private(stack)


def qualify_alternate_host(tag):
    host = "manuals.code.example.test"
    namespace = "team/manuals"
    setup = partial(prepare, host=host, namespace=namespace)
    with stack(tag, fixture_setup=setup, host=host) as (instance, upstream, approval_file, endpoint):
        repo, _ = REPOS["sha256"]
        denied = instance.exec("sync", "docs-mcp-phase2-fixture", repo, endpoint, CA, expected=None)
        assert denied.returncode and "no active HTTPS approval" in denied.stdout, denied.stdout
        assert sql(instance, "SELECT count(*) FROM git_catalog.sync_jobs") == "0"

        wrong_repo, wrong_source = REPOS["sha1"]
        register(instance, approval_file, approval(
            "sha256", host="other.code.example.test", ref_base=f"{namespace}/sha256",
            repository=wrong_repo, source=wrong_source,
        ))
        fetch(instance, wrong_repo, endpoint, reason={"upstream_connection_or_tls"})
        assert sql(instance, "SELECT count(*) FROM git_catalog.private_candidates") == "0"

        register(instance, approval_file, approval("sha256", host=host, ref_base=f"{namespace}/sha256"))
        report = fetch(instance, repo, endpoint)
        verify_candidate(instance, upstream, "sha256", report)
        assert_private(instance)
        print("Alternate HTTPS host and nested Git path passed approval and TLS hostname checks", flush=True)


def qualify(args):
    with stack(args.image_tag) as (instance, upstream, approval_file, endpoint):
        initial = {}
        for fmt, (repository, _) in REPOS.items():
            register(instance, approval_file, approval(fmt))
            port = endpoint.replace(":8443", ":8088") if fmt == "sha1" else endpoint
            report = fetch(instance, repository, port, "-" if fmt == "sha1" else CA)
            initial[fmt] = (report, *verify_candidate(instance, upstream, fmt, report))
        assert_private(instance)
        if args.smoke:
            return

        repo, source = REPOS["sha1"]
        probe = instance.exec("sync", "docs-mcp-phase2-fixture", "--confinement-probe")
        print(probe.stdout.strip(), flush=True)
        ssrf = instance.exec("sync", "docs-mcp-sync", "--reconcile", "--repository", repo, expected=None, timeout=25)
        assert ssrf.returncode and '"reason":"destination_denied"' in ssrf.stdout, ssrf.stdout
        bad_ca = instance.exec("sync", "docs-mcp-phase2-fixture", repo, endpoint,
                               "/run/secrets/db-ca.crt", expected=None, timeout=25)
        assert bad_ca.returncode and '"reason":"upstream_connection_or_tls"' in bad_ca.stdout, bad_ca.stdout
        for role in ("serve", "feed"):
            denied = instance.exec(role, "wget", "-T", "2", "-qO", "/dev/null",
                                   f"http://{endpoint.split(':')[0]}:8088/health", expected=None, timeout=5)
            assert denied.returncode, role
        original, original_path, oracle = initial["sha1"]
        origin = repo_path(upstream, "sha1")
        advance, _, _ = commit(origin, "advance", parent=oracle["initial"])
        git(origin, "update-ref", "refs/heads/main", advance)
        reused = fetch(instance, repo, endpoint)
        assert reused["evidence"]["baseline_generation"] == "0", reused
        assert reused["evidence"]["changes"]["advanced"] == 1, reused
        assert reused["evidence"]["response_bytes"] < original["evidence"]["response_bytes"] // 2, reused
        verified, _ = verify_candidate(instance, upstream, "sha1", reused)
        no_change = fetch(instance, repo, endpoint)
        assert no_change["outcome"] == "no_change" and no_change["generation"] == "1"
        assert no_change["evidence"]["logical_digest"] == reused["evidence"]["logical_digest"]
        assert no_change["observation_digest"] != reused["observation_digest"]

        forced, _, _ = commit(origin, "force replacement")
        git(origin, "update-ref", "refs/heads/main", forced)
        git(origin, "update-ref", "-d", "refs/heads/approved/release")
        git(origin, "tag", "-f", "-a", "v1", forced, "-m", "Changed tag")
        newer = fetch(instance, repo, endpoint)
        assert newer["evidence"]["changes"] == {
            "added": 0, "advanced": 0, "deleted": 1, "force_pushed": 1, "tags_moved": 1,
        }, newer
        new_path = instance.root / "staging/incoming" / newer["job"]
        assert not original_path.exists() and verified.exists() and new_path.exists()
        for key in ("initial", "optional", "hidden"):
            old = phase0.run(["git", "-c", "safe.directory=*", "--git-dir", new_path / "odb",
                              "cat-file", "-t", oracle[key]], expected=None)
            assert old.returncode != 0, key

        git(origin, "update-ref", "refs/heads/main", advance)
        for mode, reason in (
            ("redirect", {"upstream_response_denied"}), ("malformed", {"git_worker_failed", "invalid_git_protocol"}),
            ("partial", {"upstream_unavailable", "invalid_git_protocol", "git_worker_failed"}),
            ("unavailable", {"upstream_unavailable"}),
        ):
            (upstream / "mode").write_text(mode)
            failure = fetch(instance, repo, endpoint, reason=reason)
            assert new_path.exists() and verified.exists()
            if mode == "malformed":
                sample = instance.root / "staging/quarantine" / failure["job"] / "upstream-prefix"
                assert 0 < sample.stat().st_size <= 64 * 1024
        (upstream / "mode").write_text("normal")
        instance.compose("restart", "upstream")

        git(origin, "update-ref", "refs/heads/main", forced)
        pack = next((new_path / "odb/objects/pack").glob("*.pack"))
        with pack.open("r+b") as stream:
            stream.write(b"FAIL")
        fallback = fetch(instance, repo, endpoint)
        assert fallback["evidence"]["baseline_corrupt"] and fallback["evidence"]["baseline_generation"] is None
        assert fallback["evidence"]["logical_digest"] == newer["evidence"]["logical_digest"]
        assert not new_path.exists() and (instance.root / "staging/quarantine" / newer["job"]).exists()
        assert sql(instance, f"SELECT count(*) FROM git_catalog.repository_incidents WHERE repository_id='{repo}' AND reason_code='baseline_corrupt'") == "1"
        first_crash, _, _ = commit(origin, "interrupted seal", parent=forced)
        git(origin, "update-ref", "refs/heads/main", first_crash)
        interruption(instance, repo, endpoint, upstream / "mode", "sealed")
        second_crash, _, _ = commit(origin, "interrupted retirement", parent=first_crash)
        git(origin, "update-ref", "refs/heads/main", second_crash)
        interruption(instance, repo, endpoint, upstream / "mode", "committed")

        register(instance, approval_file, approval("sha1", version=2, required=["refs/heads/main", "refs/heads/missing"]))
        fetch(instance, repo, endpoint, reason={"ref_policy"})
        register(instance, approval_file, approval("sha1", version=3, response_bytes=4))
        fetch(instance, repo, endpoint, reason={"transfer_budget"})
        register(instance, approval_file, approval("sha1", version=4, pack_bytes=64))
        fetch(instance, repo, endpoint, reason={"candidate_invalid"})
        register(instance, approval_file, approval("sha1", version=5, deadline=1))
        fetch(instance, repo, endpoint, reason={"deadline"})
        register(instance, approval_file, approval("sha1", version=6))
        fetch(instance, repo, endpoint, reason={"storage_quota"}, env=("-e", "DOCS_MCP_SYNC_QUOTA_BYTES=402653184"))
        concurrent_admission(instance, repo, endpoint, upstream)
        instance.compose("run", "--rm", "--no-deps", "migrate", "--revoke-source", "--source", source)
        denied = instance.exec("sync", "docs-mcp-phase2-fixture", repo, endpoint, CA, expected=None)
        assert denied.returncode and "no active HTTPS approval" in denied.stdout, denied.stdout
        assert_private(instance)
        print("A07 and Phase 2 A08 synthetic qualification passed", flush=True)
    qualify_alternate_host(args.image_tag)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    parser.add_argument("--smoke", action="store_true")
    print(f"{datetime.now(timezone.utc).isoformat()} Phase 2 Compose qualification", flush=True)
    qualify(parser.parse_args())
