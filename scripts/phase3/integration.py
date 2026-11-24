"""Qualify Phase 3 Git publication in the hardened rootful Compose stack."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase2"))
from smart_fixtures import REPOS, approval, commit, git, repo_path

spec = importlib.util.spec_from_file_location("phase2_integration", ROOT / "scripts/phase2/integration.py")
phase2 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase2)

sql = phase2.sql
FIXTURE = "gh-docs-phase3-fixture"
INSTALL_FAULTS = (
    ("before_install_intent", "verified"),
    ("after_install_intent", "ready"),
    ("before_pack", "ready"),
    ("after_pack", "ready"),
    ("after_index", "ready"),
    ("before_refs", "ready"),
    ("after_refs", "ready"),
    ("before_manifest", "ready"),
    ("after_manifest", "ready"),
    ("before_ready", "ready"),
    ("after_ready", "ready"),
)


def obj(result):
    return next(json.loads(line) for line in result.stdout.splitlines() if line.startswith("{"))


def command(stack, repo, flag, generation=None, *, seq=None, fault=None, deny=False):
    argv = [flag, "--repository", repo]
    if generation is not None:
        argv += ["--generation", str(generation)]
    if seq is not None:
        argv += ["--sequence" if flag == "--status" else "--expect-sequence", str(seq)]
    if fault:
        argv = ["exec", "-T", "-e", f"GH_DOCS_PHASE3_FAULT={fault}", "sync",
                FIXTURE, flag, repo, *argv[3:]]
        result = stack.compose(*argv, expected=None, timeout=75)
        assert result.returncode in (137, -9), (fault, result.returncode, result.stdout)
        return None
    result = stack.exec("sync", "gh-docs-sync", *argv, expected=None if deny else 0, timeout=75)
    if deny:
        assert result.returncode, result.stdout
        return result.stdout
    return obj(result)


def read(stack, repo, generation, oid, *, expected=None, deny=False):
    result = stack.exec("serve", FIXTURE, "read", repo, str(generation), oid,
                        expected=None if deny else 0, timeout=25)
    if deny:
        assert result.returncode and "unavailable" in result.stdout, result.stdout
        return
    observed = obj(result)
    assert observed["commit"] == oid, observed
    if expected is not None:
        gid, blob, raw = expected
        assert (observed["generation"], observed["blob"], observed["size"], observed["byte_sum"]) == (
            str(gid), blob, len(raw), sum(raw)), observed
    return observed


def lifecycle(stack, repo, generation):
    return sql(stack, f"SELECT state FROM git_catalog.repository_generations "
                      f"WHERE repository_id = '{repo}' AND generation_id = {generation}")


def assert_pointer(stack, repo, generation, sequence, *, delivered=False):
    observed = command(stack, repo, "--status")
    assert observed["generation"] == (None if generation is None else str(generation)), observed
    assert observed["sequence"] == str(sequence), observed
    if generation is not None:
        event = observed["event"]
        assert event["generation"] == str(generation) and event["delivered"] == delivered, observed
    return observed


def publish(stack, repo, generation, seq):
    result = command(stack, repo, "--publish", generation, seq=seq)
    assert result["sequence"] == str(seq + 1), result
    return result


def initial(stack, root, configuration, endpoint, fmt):
    repo, _ = REPOS[fmt]
    phase2.register(stack, configuration, approval(fmt))
    fetch = phase2.fetch(stack, repo, endpoint)
    generation = int(fetch["generation"])
    oracle = json.loads((root / f"{fmt}-oracle.json").read_text())
    raw = (root / f"{fmt}-oracle.bin").read_bytes()
    assert int(oracle["size"]) == len(raw)
    command(stack, repo, "--publish", generation, seq=0, deny=True)
    command(stack, repo, "--prepare", generation)
    assert lifecycle(stack, repo, generation) == "ready"
    assert_pointer(stack, repo, None, 0)
    read(stack, repo, generation, oracle["initial"], deny=True)
    assert publish(stack, repo, generation, 0)["outcome"] == "active"
    read(stack, repo, "active", oracle["initial"], expected=(generation, oracle["blob"], raw))
    read(stack, repo, generation, oracle["hidden"], deny=True)
    return repo, generation, oracle


def new_generation(stack, upstream, repo, endpoint, identity, *, parent=None, fmt="sha1"):
    origin = repo_path(upstream, fmt)
    oid, blob, raw = commit(origin, identity, parent=parent)
    git(origin, "update-ref", "refs/heads/main", oid)
    generation = int(phase2.fetch(stack, repo, endpoint)["generation"])
    return generation, oid, blob, raw


def wait_until(callback, message, seconds=8):
    start = time.monotonic()
    while time.monotonic() - start < seconds:
        if callback():
            return
        time.sleep(0.05)
    raise AssertionError(message)


def reader_fence(stack, repo, active_oid, next_generation, sequence):
    hold = "/tmp/phase3-reader"
    stack.exec("serve", "mkdir", hold)
    argv = [*stack.compose_command, "exec", "-T", "serve", FIXTURE, "read", repo,
            "active", active_oid, "--hold", hold]
    with subprocess.Popen(argv, cwd=ROOT, env=stack.context, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, text=True) as reader:
        try:
            wait_until(lambda: stack.exec("serve", "test", "-f", f"{hold}/running",
                                          expected=None).returncode == 0, "reader worker did not start")
            assert stack.exec("serve", "sh", "-ec", "ps -o args | grep -q '[c]at-file --batch-command'").returncode == 0
            path = f"/git-store/locks/{repo}/repository.lock"
            assert stack.exec("sync", "flock", "-n", "-x", path, "true", expected=None).returncode == 1
            assert "deadline" in command(stack, repo, "--publish", next_generation, seq=sequence, deny=True)
            assert sql(stack, """SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                WHERE usename = 'gh_docs_serve' AND state = 'idle in transaction'""") == "t"
            assert stack.exec("sync", "flock", "-n", "-x", path, "true", expected=None).returncode == 1
            assert "deadline" in command(stack, repo, "--gc", deny=True)
            stack.exec("serve", "touch", f"{hold}/release")
            stdout, _ = reader.communicate(timeout=12)
            assert reader.returncode and "database connection closed" in stdout, stdout
            assert stack.exec("sync", "flock", "-n", "-x", path, "true").returncode == 0
            stack.exec("serve", "sh", "-ec", "! ps -o args | grep -q '[c]at-file --batch-command'")
        finally:
            stack.exec("serve", "touch", f"{hold}/release", expected=None)
            if reader.poll() is None:
                reader.terminate()
                reader.communicate(timeout=10)
    print("A10 database loss with live child held the filesystem fence through reap", flush=True)


def writer_fence(stack, repo, target, sequence):
    path = f"/git-store/locks/{repo}/coordinator.lock"
    argv = [*stack.compose_command, "exec", "-T", "sync", "sh", "-ec",
            'exec 9<"$1"; flock -n -x 9; echo held; read -r release', "sh", path]
    with subprocess.Popen(argv, cwd=ROOT, env=stack.context, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) as writer:
        try:
            assert writer.stdout.readline().strip() == "held"
            assert "deadline" in command(stack, repo, "--publish", target, seq=sequence, deny=True)
        finally:
            writer.communicate(input="release\n", timeout=10)
    assert writer.returncode == 0


def abort_outbox_transaction(stack, repo, generation, seq, timing):
    sql(stack, f"""
        CREATE FUNCTION public.phase3_pause_outbox() RETURNS trigger LANGUAGE plpgsql AS
          $$ BEGIN PERFORM pg_sleep(15); RETURN NEW; END $$;
        CREATE TRIGGER fixture_outbox {timing} INSERT ON git_catalog.notification_outbox
          FOR EACH ROW EXECUTE FUNCTION public.phase3_pause_outbox();
    """)
    argv = [*stack.compose_command, "exec", "-T", "sync", "gh-docs-sync", "--publish",
            "--repository", repo, "--generation", str(generation), "--expect-sequence", str(seq)]
    try:
        with subprocess.Popen(argv, cwd=ROOT, env=stack.context, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True) as child:
            wait_until(lambda: sql(stack, """SELECT count(*) FROM pg_stat_activity
                WHERE usename = 'gh_docs_sync' AND wait_event = 'PgSleep'""") == "1",
                       f"transaction did not reach {timing} outbox boundary", seconds=4)
            assert sql(stack, """SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                WHERE usename = 'gh_docs_sync' AND wait_event = 'PgSleep'""") == "t"
            stdout, _ = child.communicate(timeout=10)
            assert child.returncode, stdout
    finally:
        sql(stack, """DROP TRIGGER fixture_outbox ON git_catalog.notification_outbox;
                      DROP FUNCTION public.phase3_pause_outbox();""")


def acknowledgment(stack, repo, generation, sequence):
    event = assert_pointer(stack, repo, generation, sequence)["event"]["id"]
    update = (f"WITH ack AS (UPDATE git_catalog.notification_outbox SET delivered_at = clock_timestamp() "
              f"WHERE event_id = '{event}' AND delivered_at IS NULL RETURNING event_id) SELECT event_id FROM ack")
    assert event in sql(stack, f"BEGIN; {update}; ROLLBACK;").splitlines()
    command(stack, repo, "--recover")
    assert_pointer(stack, repo, generation, sequence)
    assert sql(stack, update) == event
    # Treat the first success response as lost and exercise idempotent repeat.
    assert not sql(stack, update)
    command(stack, repo, "--recover")
    assert_pointer(stack, repo, generation, sequence, delivered=True)


def pack_read(stack, repo, oid, *, present=True):
    tested = stack.exec("sync", "env", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null",
                        "GIT_NO_REPLACE_OBJECTS=1", "LC_ALL=C", "git",
                        "--git-dir", f"/git-store/repositories/{repo}", "cat-file", "-t", oid,
                        expected=None)
    if present:
        assert tested.returncode == 0 and tested.stdout.strip() == "commit", (oid, tested.stdout)
    else:
        assert tested.returncode and "could not get object info" in tested.stdout, (oid, tested.stdout)


def install_boundaries(stack, upstream, endpoint, repo, generation, oid):
    for point, expected_state in INSTALL_FAULTS:
        target, _, _, _ = new_generation(stack, upstream, repo, endpoint, f"install fault {point}")
        command(stack, repo, "--prepare", target, fault=point)
        assert_pointer(stack, repo, generation, 1)
        read(stack, repo, "active", oid)
        command(stack, repo, "--recover")
        command(stack, repo, "--recover")
        assert lifecycle(stack, repo, target) == expected_state, point
        read(stack, repo, target, oid, deny=True)
        command(stack, repo, "--discard", target)
    print("A09 verified prepare recovery at every install, roots, manifest and ready boundary", flush=True)


def retention(stack, root, endpoint, repo, first, oracle):
    origin = repo_path(root, "sha1")
    git(origin, "update-ref", "-d", "refs/heads/approved/release")
    git(origin, "update-ref", "-d", "refs/tags/v1")
    second, second_oid, blob, raw = new_generation(stack, root, repo, endpoint, "force push")
    command(stack, repo, "--prepare", second)
    read(stack, repo, second, second_oid, deny=True)
    writer_fence(stack, repo, second, 1)
    reader_fence(stack, repo, oracle["initial"], second, 1)
    command(stack, repo, "--publish", second, seq=1, fault="before_publish")
    assert_pointer(stack, repo, first, 1)
    for timing in ("BEFORE", "AFTER"):
        abort_outbox_transaction(stack, repo, second, 1, timing)
        assert_pointer(stack, repo, first, 1)
        assert lifecycle(stack, repo, second) == "ready"
    command(stack, repo, "--publish", second, seq=1, fault="after_publish")
    assert_pointer(stack, repo, second, 2)
    assert command(stack, repo, "--status", seq=1)["event"]["generation"] == str(first)
    assert "sequence mismatch" in command(stack, repo, "--publish", second, seq=1, deny=True)
    with ThreadPoolExecutor(max_workers=2) as pool:
        old = pool.submit(read, stack, repo, first, oracle["optional"])
        live = pool.submit(read, stack, repo, "active", second_oid, expected=(second, blob, raw))
        assert old.result()["generation"] == str(first)
        assert live.result()["sequence"] == "2"
    read(stack, repo, "active", oracle["initial"], deny=True)
    assert command(stack, repo, "--rollback", first, seq=2)["sequence"] == "3"
    read(stack, repo, "active", oracle["initial"])
    assert command(stack, repo, "--rollback", second, seq=3)["sequence"] == "4"
    acknowledgment(stack, repo, second, 4)

    generations = [first, second]
    previous = second_oid
    for seq in range(4, 8):
        generation, oid, _, _ = new_generation(stack, root, repo, endpoint, f"retained {seq}",
                                               parent=previous)
        command(stack, repo, "--prepare", generation)
        publish(stack, repo, generation, seq)
        generations.append(generation)
        previous = oid

    sql(stack, f"""UPDATE git_catalog.repository_generations SET demoted_at = clock_timestamp() - interval '25 hours'
        WHERE repository_id = '{repo}' AND state = 'retained' AND generation_id <> {second}""")
    command(stack, repo, "--gc", fault="before_retire")
    command(stack, repo, "--recover")
    assert lifecycle(stack, repo, first) == "retained"
    command(stack, repo, "--gc", fault="after_retire")
    assert lifecycle(stack, repo, first) == "retired"
    assert (stack.root / f"store/repositories/{repo}/refs/gh-docs/retention/{first}").is_dir()
    read(stack, repo, first, oracle["optional"], deny=True)
    command(stack, repo, "--recover")
    assert not (stack.root / f"store/repositories/{repo}/refs/gh-docs/retention/{first}").exists()
    young = command(stack, repo, "--gc")
    assert not young["retired"], young
    pack_read(stack, repo, oracle["optional"])
    sql(stack, f"""UPDATE git_catalog.repository_generations SET demoted_at = clock_timestamp() - interval '25 hours'
        WHERE repository_id = '{repo}' AND generation_id = {second}""")
    command(stack, repo, "--gc", fault="after_root_removal")
    command(stack, repo, "--recover")
    assert lifecycle(stack, repo, second) == "retired"
    read(stack, repo, "active", second_oid)

    pack_dir = stack.root / f"store/repositories/{repo}/objects/pack"
    ancient = time.time() - 16 * 24 * 3600

    def stage_expired(identity):
        prior = set(pack_dir.glob("*.pack"))
        generation, oid, _, _ = new_generation(stack, root, repo, endpoint, identity)
        command(stack, repo, "--prepare", generation)
        packs = set(pack_dir.glob("*.pack")) - prior
        assert len(packs) == 1, packs
        for pack in packs:
            os.utime(pack, times=(ancient, ancient))
        command(stack, repo, "--discard", generation)
        pack_read(stack, repo, oid)
        return oid

    expired = stage_expired("aged discarded candidate")
    for point in ("before_repack", "after_repack", "after_repack_intent", "before_repack_install",
                  "after_repack_install", "before_old_pack_removal", "during_old_pack_removal",
                  "after_old_pack_removal"):
        if point == "during_old_pack_removal":
            expired = stage_expired("second aged discarded candidate")
        command(stack, repo, "--gc", fault=point)
        command(stack, repo, "--recover")
        assert_pointer(stack, repo, generations[-1], 8)
        read(stack, repo, "active", previous)
        if point not in ("before_repack", "after_repack"):
            pack_read(stack, repo, expired, present=False)
    for generation in generations[2:-1]:
        assert lifecycle(stack, repo, generation) == "retained"
    pack_read(stack, repo, oracle["optional"])
    pack_read(stack, repo, second_oid)
    print("A09–A11 publication, outbox, rollback, retention, and repack interruption recovery passed", flush=True)


def integrity(stack, root, endpoint, repo, generation, oracle):
    target, _, _, _ = new_generation(stack, root, repo, endpoint, "corrupt target", fmt="sha256")
    job = sql(stack, f"SELECT job_id FROM git_catalog.private_candidates WHERE repository_id='{repo}' AND generation_id={target}")
    private_pack = next((stack.root / "staging/incoming" / job / "odb/objects/pack").glob("*.pack"))
    verified = private_pack.read_bytes()
    private_pack.write_bytes(b"BAD!" + verified[4:])
    command(stack, repo, "--prepare", target, deny=True)
    assert not assert_pointer(stack, repo, generation, 1)["blocked"]
    private_pack.write_bytes(verified)
    command(stack, repo, "--prepare", target)
    published_pack = stack.root / f"store/repositories/{repo}/objects/pack" / private_pack.name
    published_pack.unlink()
    command(stack, repo, "--publish", target, seq=1, deny=True)
    assert assert_pointer(stack, repo, generation, 1)["blocked"]
    read(stack, repo, "active", oracle["initial"], deny=True)
    command(stack, repo, "--recover", deny=True)
    published_pack.write_bytes(verified)
    os.chown(published_pack, 10002, 10002)
    os.chmod(published_pack, 0o644)
    command(stack, repo, "--recover")
    assert not assert_pointer(stack, repo, generation, 1)["blocked"]
    assert sql(stack, f"SELECT count(*) FROM git_catalog.repository_incidents WHERE repository_id = '{repo}'") == "3"
    unknown = stack.root / f"store/repositories/{repo}/objects/unexpected"
    unknown.write_text("preserve ambiguous data")
    command(stack, repo, "--recover", deny=True)
    assert unknown.read_text() == "preserve ambiguous data"
    unknown.unlink()
    command(stack, repo, "--recover")
    pack_dir = published_pack.parent
    stray = pack_dir / ("pack-" + "0" * 64 + ".pack")
    stray.write_bytes(verified)
    stray_index = stray.with_suffix(".idx")
    stray_index.write_bytes(published_pack.with_suffix(".idx").read_bytes())
    for file in (stray, stray_index):
        os.chown(file, 10002, 10002)
    command(stack, repo, "--recover", deny=True)
    assert stray.exists() and stray_index.exists()
    stray.unlink()
    stray_index.unlink()
    command(stack, repo, "--recover")
    print("A08 malformed private packs, missing canonical packs, and unknown data failed closed", flush=True)
    return target


def grants(stack):
    assert sql(stack, """SELECT
        has_table_privilege('gh_docs_serve','git_catalog.readable_refs','SELECT'),
        has_table_privilege('gh_docs_serve','git_catalog.generation_refs','SELECT'),
        has_table_privilege('gh_docs_serve','git_catalog.publication_work','SELECT'),
        has_table_privilege('gh_docs_sync','git_catalog.notification_outbox','UPDATE'),
        has_function_privilege('gh_docs_serve','git_catalog.promote_generation(uuid,bigint,bigint,boolean)','EXECUTE')
        """) == "t|f|f|f|f"
    stack.exec("serve", "sh", "-ec", "test ! -r /git-store/.publication && test ! -e /git-sync-private")
    stack.exec("feed", "sh", "-ec", "test ! -e /git-store")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    subset = parser.add_mutually_exclusive_group()
    subset.add_argument("--smoke", action="store_true")
    subset.add_argument("--retention-only", action="store_true")
    args = parser.parse_args()
    print(f"{datetime.now(timezone.utc).isoformat()} Phase 3 Compose qualification", flush=True)
    with phase2.stack(args.image_tag, publication=True) as (stack, upstream, configuration, endpoint):
        sha1 = initial(stack, upstream, configuration, endpoint, "sha1")
        sha256 = initial(stack, upstream, configuration, endpoint, "sha256")
        grants(stack)
        if args.smoke:
            print("Phase 3 initial publication smoke passed for SHA-1 and SHA-256", flush=True)
            return
        if not args.retention_only:
            install_boundaries(stack, upstream, endpoint, *sha1[:2], sha1[2]["initial"])
        retention(stack, upstream, endpoint, *sha1)
        if args.retention_only:
            return
        ready = integrity(stack, upstream, endpoint, *sha256)
        phase2.register(stack, configuration, approval("sha256", version=2))
        read(stack, sha256[0], "active", sha256[2]["initial"], deny=True)
        assert "publication target unavailable" in command(stack, sha256[0], "--publish", ready, seq=1, deny=True)
        assert sql(stack, "SELECT count(*) FROM git_catalog.visible_repositories") == "1"
        stack.compose("run", "--rm", "--no-deps", "migrate", "--revoke-source", "--source", REPOS["sha1"][1])
        assert sql(stack, "SELECT count(*) FROM git_catalog.visible_repositories") == "0"
        retained = sql(stack, f"""SELECT generation_id FROM git_catalog.repository_generations
            WHERE repository_id='{sha1[0]}' AND state='retained'
            ORDER BY last_publication_sequence DESC LIMIT 1""")
        assert "publication target unavailable" in command(stack, sha1[0], "--rollback", retained, seq=8, deny=True)
        print("Current approval gates reads, promotion, and rollback", flush=True)


if __name__ == "__main__":
    main()
