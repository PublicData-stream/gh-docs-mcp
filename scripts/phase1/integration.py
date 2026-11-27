#!/usr/bin/env python3
"""Qualify Phase 1 with disposable rootful Compose projects."""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import re
import secrets
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile

from native_fixtures import generate, own_tree

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase0"))
import firewall
import integration as phase0

FIXTURE = ["docs-mcp-phase1-fixture"]
ORACLE = "/phase1-oracle.json"
CATALOG_VERSIONS = ["2", "3", "4", "5", "6", "7", "8", "9"]
RUN = phase0.run
DOCKER = phase0.DOCKER


@contextmanager
def stack(tag, variant, fixtures=False):
    project = f"docsmcp1-{variant}-{secrets.token_hex(4)}"
    root = Path(tempfile.mkdtemp(prefix=f"{project}-", dir="/tmp"))
    table = project.replace("-", "_")
    instance = None
    installed = False
    try:
        instance = phase0.Stack(root, project, tag, mcp_fixture=fixtures)
        if fixtures:
            records = generate(root / "store", root / "oracle.json", root / "staging/incoming")
            own_tree(root / "store/repositories")
            own_tree(root / "staging/incoming")
            for record in records:
                (root / "staging/incoming" / record["job"]).chmod(0o700)
                print(f'fixture {record["format"]} merge {record["merge"]}', flush=True)
            override = root / "phase1.json"
            override.write_text(json.dumps({"services": {
                role: {
                    "image": f"docs-mcp-phase1-{role}:{tag}",
                    "volumes": [{
                        "type": "bind", "source": str(root / "oracle.json"),
                        "target": ORACLE, "read_only": True,
                    }],
                } for role in ("serve", "sync")
            }}))
            instance.compose_command[6:6] = ["-f", str(override)]
        instance.create()
        firewall.install(table, instance.env, root)
        installed = True
        yield instance
    except BaseException:
        if instance is not None and instance.ids:
            print(instance.compose("logs", "--no-color", "--tail", "12", expected=None).stdout, flush=True)
        raise
    finally:
        try:
            if instance is not None:
                instance.compose("down", "--volumes", "--remove-orphans", timeout=60)
            if installed:
                firewall.remove(table)
            shutil.rmtree(root)
        except (OSError, RuntimeError, subprocess.TimeoutExpired):
            print(f"Cleanup incomplete for {project}, fixtures at {root}, firewall table {table}", flush=True)
            raise


def sql(stack, statements):
    return stack.exec("postgres", "psql", "-X", "-v", "ON_ERROR_STOP=1", "-Atq",
                      "-U", "postgres", "-d", "gh_docs", input=statements).stdout.strip()


def role(stack, service, mode, *args, **options):
    return stack.exec(service, *FIXTURE, mode, ORACLE, *args, **options)


def upgrade(tag):
    with stack(tag, "upgrade") as old:
        old.one_shot("volume-admin")
        old.compose("up", "--no-build", "--pull", "never", "--wait", "--wait-timeout", "65",
                    "postgres", timeout=75)
        foundation = (ROOT / "migrations/0001_foundation.sql").read_text()
        catalog = (ROOT / "migrations/0002_git_catalog.sql").read_text()
        synchronization = (ROOT / "migrations/0003_synchronization.sql").read_text()
        assert sql(old, f"SET ROLE gh_docs_migrate;\n{foundation}").splitlines() == []
        assert sql(old, "SELECT to_regnamespace('git_catalog') IS NULL;") == "t"
        sql(old, "SET ROLE gh_docs_migrate; INSERT INTO foundation.probe VALUES (72, 'upgrade sentinel');")
        assert sql(old, f"SET ROLE gh_docs_migrate; BEGIN;\n{catalog}\nROLLBACK;\n"
                        "SELECT to_regnamespace('git_catalog') IS NULL;") == "t"
        sql(old, f"SET ROLE gh_docs_migrate;\n{catalog}")
        legacy_repo = "11111111-1111-4111-8111-111111111111"
        legacy_source = "22222222-2222-4222-8222-222222222222"
        sql(old, f"SET ROLE gh_docs_sync; SELECT git_catalog.register_fixture("
            f"'{legacy_repo}','{legacy_source}','sha1','urn:gh-docs:fixture:{legacy_repo}',"
            "ARRAY[convert_to('refs/heads/main','UTF8')]);")
        sql(old, f"SET ROLE gh_docs_migrate;\n{synchronization}")
        verified_repo = "11111111-1111-4111-8111-111111111112"
        verified_source = "22222222-2222-4222-8222-222222222223"
        sql(old, f"""SET ROLE gh_docs_migrate;
            SELECT git_catalog.approve_source('{verified_repo}','{verified_source}','sha1',
                'https://fixture.example.invalid/repo.git', 1, ARRAY[convert_to('refs/heads/main','UTF8')],
                jsonb_build_object('reviewed_at','2002-02-02T02:02:02Z'));
            SET ROLE gh_docs_sync;
            SELECT git_catalog.seal_candidate(git_catalog.start_reconcile('{verified_repo}','{verified_source}',1),
                decode(repeat('aa',32),'hex'),decode(repeat('bb',32),'hex'),decode(repeat('cc',32),'hex'),
                convert_to('refs/heads/main','UTF8'),
                jsonb_build_array(jsonb_build_array(encode(convert_to('refs/heads/main','UTF8'),'base64'),
                    'commit',repeat('01',20),repeat('01',20))), '{{}}', '2002-02-02T02:02:02Z', 1);
        """)
        legacy_verified = sql(old, f"""SELECT g.generation_id, g.state, encode(g.manifest_digest,'hex'),
            p.state, encode(f.target_oid, 'hex')
            FROM git_catalog.repository_generations g
            JOIN git_catalog.private_candidates p USING (repository_id, generation_id)
            JOIN git_catalog.generation_refs f USING (repository_id, generation_id)
            WHERE g.repository_id = '{verified_repo}'""")
        assert legacy_verified == f"0|verified|{'bb' * 32}|usable|{'01' * 20}", legacy_verified
        old.one_shot("migrate")
        old.one_shot("migrate")
        assert sql(old, "SELECT value FROM foundation.probe WHERE id = 72;") == "upgrade sentinel"
        assert sql(old, f"SELECT canonical_identity FROM git_catalog.repository_sources WHERE source_id='{legacy_source}';") == f"urn:gh-docs:fixture:{legacy_repo}"
        assert sql(old, "SELECT version FROM git_catalog.migrations ORDER BY version;").splitlines() == CATALOG_VERSIONS
        assert sql(old, f"""SELECT g.generation_id, g.state, encode(g.manifest_digest,'hex'),
            p.state, encode(f.target_oid, 'hex')
            FROM git_catalog.repository_generations g
            JOIN git_catalog.private_candidates p USING (repository_id, generation_id)
            JOIN git_catalog.generation_refs f USING (repository_id, generation_id)
            WHERE g.repository_id = '{verified_repo}'""") == legacy_verified
        assert sql(old, "SELECT count(*) FROM git_catalog.readable_generations") == "0"
    print("A05 Phase 0 and Phase 2 upgrades, verified candidate preservation, and repeated migration passed", flush=True)


def database_constraints(stack):
    actual = sql(stack, """
        SELECT count(*) FROM git_catalog.repository_generations WHERE state = 'verified';
        SELECT count(*) FROM git_catalog.active_generations;
        SELECT count(*) FROM git_catalog.notification_outbox;
        SELECT count(*) = count(DISTINCT advisory_key) AND min(advisory_key) >= 1024
        FROM git_catalog.lock_keys;
        SELECT uuid_extract_version(uuidv7()) = 7;
    """).splitlines()
    assert actual == ["4", "0", "0", "t", "t"], actual
    sql(stack, """
        DO $$
        DECLARE
            first_repo constant uuid := '11111111-1111-4111-8111-111111111111';
            second_repo constant uuid := '33333333-3333-4333-8333-333333333333';
            second_source constant uuid := '44444444-4444-4444-8444-444444444444';
        BEGIN
            BEGIN
                INSERT INTO git_catalog.repositories (repository_id, object_format)
                    VALUES ('11111111-1111-7111-8111-111111111111', 'sha1');
                RAISE EXCEPTION 'accepted non-v4 repository';
            EXCEPTION WHEN check_violation THEN NULL;
            END;
            BEGIN
                INSERT INTO git_catalog.repository_generations
                    (repository_id, generation_id, source_id, object_format, policy_version, provenance, observed_at)
                    VALUES (first_repo, 300, second_source, 'sha1', 1, '{}', now());
                RAISE EXCEPTION 'accepted cross-scope source';
            EXCEPTION WHEN foreign_key_violation THEN NULL;
            END;
            BEGIN
                INSERT INTO git_catalog.generation_refs
                    (repository_id, generation_id, object_format, ref_name, target_oid, target_type)
                    VALUES (first_repo, 0, 'sha256', 'refs/tags/fk', decode(repeat('01', 32), 'hex'), 'tag');
                RAISE EXCEPTION 'accepted cross-format ref';
            EXCEPTION WHEN foreign_key_violation THEN NULL;
            END;
            BEGIN
                INSERT INTO git_catalog.generation_refs
                    (repository_id, generation_id, object_format, ref_name, target_oid, target_type)
                    VALUES (second_repo, 0, 'sha256', 'refs/tags/oid', decode(repeat('01', 20), 'hex'), 'tag');
                RAISE EXCEPTION 'accepted invalid native OID length';
            EXCEPTION WHEN check_violation THEN NULL;
            END;
            BEGIN
                INSERT INTO git_catalog.active_generations
                    VALUES (first_repo, 300, 0, now());
                RAISE EXCEPTION 'accepted missing generation';
            EXCEPTION WHEN foreign_key_violation THEN NULL;
            END;
        END $$;
    """)
    assert sql(stack, """
        BEGIN;
        UPDATE git_catalog.repositories SET approval_status = 'approved';
        INSERT INTO git_catalog.active_generations (repository_id, generation_id, publication_sequence, published_at)
            SELECT repository_id, generation_id, 0, now() FROM git_catalog.repository_generations
            WHERE generation_id = 0;
        SELECT count(*) FROM git_catalog.visible_repositories;
        ROLLBACK;
    """) == "0"
    role(stack, "serve", "serve-db")
    print("A05 scoped catalog, empty active pointers, UUID/OID checks, and grants passed", flush=True)


def filesystem(stack):
    role(stack, "sync", "prepare-locks")
    records = json.loads((stack.root / "oracle.json").read_bytes())
    lock_dir = stack.root / "store/locks" / records[0]["repo"]
    lock = lock_dir / "repository.lock"
    inode = lock.stat().st_ino
    role(stack, "sync", "prepare-locks")
    assert lock.stat().st_ino == inode
    result = stack.exec("serve", "sh", "-ec",
                        'test ! -e /git-sync-private && ! touch "/git-store/write-phase1"')
    assert "Read-only file system" in result.stdout, result.stdout
    stack.exec("sync", "sh", "-ec", "test -r /git-sync-private/incoming")
    with subprocess.Popen(
        [*stack.compose_command, "exec", "-T", "serve", *FIXTURE, "fence-probe", ORACLE],
        env=stack.context, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, cwd=ROOT,
    ) as holder:
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(holder.stdout, selectors.EVENT_READ)
                assert selector.select(5), "reader did not acquire the repository fence"
                assert holder.stdout.readline().strip() == f'{records[0]["repo"]} locked'
            denied = role(stack, "sync", "exclusive-probe", expected=None)
            assert denied.returncode != 0 and "deadline" in denied.stdout, denied.stdout
        finally:
            holder.stdin.close()
            try:
                holder.wait(timeout=7)
            except subprocess.TimeoutExpired:
                holder.kill()
                holder.wait()
        assert holder.returncode == 0
    role(stack, "sync", "exclusive-probe")


def invalid_store_inputs(stack):
    record = json.loads((stack.root / "oracle.json").read_bytes())[0]
    repo = stack.root / "store/repositories" / record["repo"]

    def rejects():
        role(stack, "serve", "reject-store", record["repo"])

    for relative in ("objects/info/alternates", "info/grafts", "shallow", "objects/pack/pack-invalid.promisor"):
        path = repo / relative
        path.write_bytes(b"synthetic disallowed sidecar\n")
        os.chown(path, 10002, 10002)
        try:
            rejects()
        finally:
            path.unlink()
    config = repo / "config"
    config_bytes = config.read_bytes()
    config.write_bytes(config_bytes + b'[include]\n path = ../outside\n')
    try:
        rejects()
    finally:
        config.write_bytes(config_bytes)
    for path, create in [
        (repo / "refs/heads/symlink", lambda path: path.symlink_to(repo / "HEAD")),
        (repo / "refs/heads/hardlink", lambda path: path.hardlink_to(repo / "HEAD")),
        (repo / "refs/heads/fifo", os.mkfifo),
    ]:
        create(path)
        try:
            rejects()
        finally:
            path.unlink()
    with (repo / "HEAD").open("rb") as head:
        os.fchown(head.fileno(), 0, 0)
        try:
            rejects()
        finally:
            os.fchown(head.fileno(), 10002, 10002)


def fresh(args):
    with stack(args.image_tag, "fresh", fixtures=True) as current:
        current.start()
        assert sql(current, "SELECT version FROM git_catalog.migrations ORDER BY version;").splitlines() == CATALOG_VERSIONS
        filesystem(current)
        for mode, service in [("read", "serve"), ("fail-setup", "serve"), ("register", "sync")]:
            print(role(current, service, mode, timeout=80).stdout, end="", flush=True)
        database_constraints(current)
        invalid_store_inputs(current)
        print(role(current, "serve", "read", timeout=60).stdout, end="", flush=True)
        phase0.mcp_tests(current, args.node, args.client_dir)
    print("A05–A06 Phase 1 Compose fixture qualification passed", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True, help="tag of the five locally rebuilt role and fixture targets")
    parser.add_argument("--client-dir", required=True, type=Path)
    parser.add_argument("--node", required=True, type=Path)
    args = parser.parse_args()
    if not __debug__ or os.geteuid() != 0 or int(os.getenv("SUDO_UID", "0")) == 0:
        parser.error("run with assertions enabled and sudo for scoped nftables and fixture ownership")
    args.client_dir = args.client_dir.resolve(strict=True)
    if not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,63}", args.image_tag):
        parser.error("invalid local image tag")
    if not args.client_dir.is_relative_to("/tmp") or not args.node.is_absolute():
        parser.error("client-dir must be under /tmp and node must be absolute")
    print(f"{datetime.now(timezone.utc).isoformat()} {platform.platform()} Phase 1 qualification", flush=True)
    print(RUN(["git", "-c", f"safe.directory={ROOT}", "rev-parse", "HEAD"]).stdout.strip(), flush=True)
    print(RUN([*DOCKER, "version", "--format", "{{.Server.Version}} {{.Server.Arch}}"]).stdout.strip(), flush=True)
    print(RUN([*DOCKER, "compose", "version"]).stdout.strip(), flush=True)
    images = [f"docs-mcp-{name}:{args.image_tag}" for name in ("serve", "sync", "feed", "postgres")]
    images += [f"docs-mcp-phase1-{name}:{args.image_tag}" for name in ("serve", "sync")]
    print(RUN([*DOCKER, "image", "inspect", "--format", "{{.Id}} {{join .RepoTags \", \"}}", *images]).stdout,
          end="", flush=True)
    upgrade(args.image_tag)
    fresh(args)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, phase0.terminate)
    main()
