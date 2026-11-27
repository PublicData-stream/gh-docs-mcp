#!/usr/bin/env python3
"""Capture or restore a quiesced rootful Compose catalog and canonical store."""

import argparse
from contextlib import contextmanager, ExitStack
from hashlib import file_digest
import fcntl
import json
import os
from pathlib import Path
import re
import select
import stat
import subprocess
import sys
import time
from uuid import UUID


CATALOG = """
SELECT jsonb_build_object(
    'schema', (SELECT max(version) FROM git_catalog.migrations),
    'repositories', coalesce((SELECT jsonb_agg(to_jsonb(r) ORDER BY repository_id)
                              FROM git_catalog.repositories r), '[]'),
    'pointers', coalesce((SELECT jsonb_agg(to_jsonb(a) ORDER BY repository_id)
                          FROM git_catalog.active_generations a), '[]'),
    'protected', coalesce((SELECT jsonb_agg(jsonb_build_array(g.repository_id, g.generation_id,
        g.state, g.source_id, g.policy_version, encode(g.manifest_digest, 'hex'))
        ORDER BY g.repository_id, g.generation_id)
        FROM git_catalog.repository_generations g WHERE g.state IN ('active', 'retained', 'ready')), '[]'),
    'unsettled', EXISTS (SELECT FROM git_catalog.publication_work)
              OR EXISTS (SELECT FROM git_catalog.publication_blocks)
              OR EXISTS (SELECT FROM git_catalog.repository_generations
                         WHERE state IN ('candidate', 'verified'))
              OR EXISTS (SELECT FROM git_catalog.sync_jobs WHERE state = 'running')
              OR EXISTS (SELECT FROM git_catalog.scheduled_jobs WHERE state = 'running')
)
"""
KEYS = """
SELECT coalesce(jsonb_agg(jsonb_build_array(repository_id, scope, generation_id, advisory_key)
    ORDER BY repository_id, CASE scope WHEN 'coordinator' THEN 0 WHEN 'repository' THEN 1 ELSE 2 END,
    generation_id), '[]') FROM git_catalog.lock_keys
"""
SQL = ["psql", "-XqAt", "-v", "ON_ERROR_STOP=1", "-U", "postgres", "-d", "gh_docs"]
SUPPORTED_SCHEMAS = (7, 8)


def sha256(path):
    with path.open("rb") as data:
        return file_digest(data, "sha256").hexdigest()


def metadata(path):
    info = path.lstat()
    return {"uid": info.st_uid, "gid": info.st_gid, "mode": stat.S_IMODE(info.st_mode)}


def inventory(root):
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("store root must be a directory")
    result = {".": {"kind": "directory", **metadata(root)}}
    for directory, dirs, files in os.walk(root):
        for name in sorted(dirs + files):
            path = Path(directory) / name
            info = path.lstat()
            entry = metadata(path)
            if stat.S_ISDIR(info.st_mode):
                entry["kind"] = "directory"
            elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                entry.update(kind="file", size=info.st_size, sha256=sha256(path))
            else:
                raise ValueError("unexpected canonical entry type")
            result[path.relative_to(root).as_posix()] = entry
    return result


def copy_store(source, target, entries):
    if not target.exists():
        target.mkdir(mode=0o700)
    if not stat.S_ISDIR(target.lstat().st_mode) or any(target.iterdir()):
        raise ValueError("destination store must be an empty directory")
    for relative, entry in sorted(entries.items()):
        if relative == ".":
            continue
        path = target / relative
        if entry["kind"] == "directory":
            path.mkdir(mode=0o700)
        else:
            with (source / relative).open("rb") as original, path.open("xb") as copy:
                while chunk := original.read(1024 * 1024):
                    copy.write(chunk)
                copy.flush()
                os.fchown(copy.fileno(), entry["uid"], entry["gid"])
                os.fchmod(copy.fileno(), entry["mode"])
                os.fsync(copy.fileno())
    for relative, entry in sorted(entries.items(), reverse=True):
        if entry["kind"] == "directory":
            path = target / relative
            os.chown(path, entry["uid"], entry["gid"])
            path.chmod(entry["mode"])
            sync_directory(path)
    if inventory(target) != entries:
        raise ValueError("copied store differs from its inventory")


def sync_directory(path):
    fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def verify(bundle, expected):
    manifest = bundle / "manifest.json"
    dump = bundle / "catalog.dump"
    for entry in (manifest, dump):
        info = entry.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise ValueError("invalid backup file type")
    if sha256(manifest) != expected:
        raise ValueError("backup manifest digest mismatch")
    record = json.loads(manifest.read_text())
    if record["version"] != 1 or record["catalog"]["schema"] not in SUPPORTED_SCHEMAS:
        raise ValueError("unsupported backup version or catalog schema")
    if sha256(dump) != record["dump_sha256"] or inventory(bundle / "store") != record["store"]:
        raise ValueError("backup does not match the verified inventory")
    return record


class Project:
    def __init__(self, name, files, env=None):
        self.env = env
        self.command = ["docker", "--context", "default", "compose", "-p", name]
        for file in files:
            self.command += ["-f", str(file)]

    def run(self, *args, **options):
        return subprocess.run([*self.command, *args], env=self.env, check=True, timeout=600, **options)

    def postgres(self, *args, **options):
        return self.run("exec", "-T", "postgres", *args, **options)

    def assert_offline(self):
        result = self.run("--profile", "*", "ps", "--status", "running", "--format", "json",
                          capture_output=True, text=True)
        for line in result.stdout.splitlines():
            item = json.loads(line)
            if item["Service"] not in {"postgres", "sink", "upstream"}:
                raise ValueError("stop all application, proxy and operator containers first")

    @contextmanager
    def session(self):
        process = subprocess.Popen([*self.command, "exec", "-T", "postgres", *SQL],
                                   env=self.env, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        try:
            yield process
        finally:
            try:
                process.stdin.close()
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            finally:
                process.stdout.close()

    def store(self):
        resolved = self.run("config", "--format", "json", "sync",
                            capture_output=True, text=True)
        mounts = json.loads(resolved.stdout)["services"]["sync"]["volumes"]
        mount, = (item for item in mounts if item["target"] == "/git-store"
                  and item["type"] == "bind" and not item.get("read_only"))
        return Path(mount["source"])


def query(connection, sql):
    connection.stdin.write(sql.strip().rstrip(";") + ";\n")
    connection.stdin.flush()
    readable, _, _ = select.select([connection.stdout], [], [], 15)
    if not readable:
        raise TimeoutError("maintenance database query did not respond")
    line = connection.stdout.readline()
    if not line:
        raise RuntimeError("maintenance database session closed")
    return line.rstrip("\n")


def acquire(connection, scope):
    if query(connection, f"SELECT pg_try_advisory_lock({int(scope)})") != "t":
        raise ValueError("database fence is busy")


def freeze(connection, handles, store):
    acquire(connection, 1)
    keys_before = query(connection, KEYS)
    for repository, scope, generation, key in json.loads(keys_before):
        name = f"generation-{int(generation)}.lock" if scope == "generation" else f"{scope}.lock"
        if scope not in {"coordinator", "repository", "generation"}:
            raise ValueError("unknown filesystem fence")
        directory = store / "locks" / str(UUID(repository))
        path = directory / name
        if not directory.exists():
            # A catalog-only repository can have keys before its first store initialization.
            acquire(connection, key)
            continue
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        handles.callback(os.close, fd)
        found = os.fstat(fd)
        if not stat.S_ISREG(found.st_mode) or found.st_nlink != 1:
            raise ValueError("invalid filesystem fence")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        acquire(connection, key)
    relations = query(connection, """
        SELECT string_agg(format('%I.%I', schemaname, tablename), ', ' ORDER BY schemaname, tablename)
        FROM pg_tables WHERE schemaname IN ('foundation', 'git_catalog')
    """)
    if not relations:
        raise ValueError("catalog tables are absent")
    # SHARE prevents DML and DDL while allowing pg_dump's ACCESS SHARE locks.
    query(connection, f"BEGIN; LOCK TABLE {relations} IN SHARE MODE NOWAIT; SELECT 'frozen'")
    if query(connection, KEYS) != keys_before:
        raise ValueError("catalog fence inventory changed during acquisition")


def backup(project, store, bundle):
    project.assert_offline()
    if bundle.exists() or bundle.is_relative_to(store) or store.is_relative_to(bundle):
        raise ValueError("use a new backup directory outside the canonical store")
    start = time.monotonic()
    with ExitStack() as handles, project.session() as connection:
        freeze(connection, handles, store)
        catalog = json.loads(query(connection, CATALOG))
        if catalog["schema"] not in SUPPORTED_SCHEMAS:
            raise ValueError("unsupported catalog schema for backup")
        if catalog["unsettled"]:
            raise ValueError("settle publication work and private candidates before backup")
        project.assert_offline()
        bundle.mkdir(mode=0o700)
        dump = bundle / "catalog.dump"
        with dump.open("xb") as stream:
            project.postgres("pg_dump", "-Fc", "-U", "postgres", "-d", "gh_docs", stdout=stream)
            stream.flush()
            os.fsync(stream.fileno())
        entries = inventory(store)
        copy_store(store, bundle / "store", entries)
        if json.loads(query(connection, CATALOG)) != catalog or inventory(store) != entries:
            raise ValueError("source changed while snapshot was fenced")
        manifest = bundle / "manifest.json"
        with manifest.open("x") as stream:
            json.dump({"version": 1, "catalog": catalog, "dump_sha256": sha256(dump),
                       "store": entries}, stream, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        sync_directory(bundle)
        sync_directory(bundle.parent)
        digest = sha256(manifest)
        verify(bundle, digest)
    return {"manifest_sha256": digest, "seconds": time.monotonic() - start,
            "bytes": dump.stat().st_size + sum(entry.get("size", 0) for entry in entries.values())}


def restore(project, store, bundle, expected):
    start = time.monotonic()
    project.assert_offline()
    record = verify(bundle, expected)
    checked = time.monotonic()
    if not stat.S_ISDIR(store.lstat().st_mode) or any(store.iterdir()):
        raise ValueError("restore requires an empty and initialized canonical mount")
    with project.session() as connection:
        acquire(connection, 1)
        if query(connection, "SELECT to_regnamespace('git_catalog') IS NULL "
                             "AND to_regnamespace('foundation') IS NULL") != "t":
            raise ValueError("restore requires a fresh provisioned database")
        # Finish the verified store before the catalog can admit any reader.
        copy_store(bundle / "store", store, record["store"])
        if verify(bundle, expected) != record:
            raise ValueError("backup changed during restore")
        with (bundle / "catalog.dump").open("rb") as stream:
            project.postgres("pg_restore", "--single-transaction", "--exit-on-error",
                             "-U", "postgres", "-d", "gh_docs", stdin=stream)
        if json.loads(query(connection, CATALOG)) != record["catalog"]:
            raise ValueError("restored catalog differs from backup")
        project.assert_offline()
    for repository in record["catalog"]["repositories"]:
        identity = str(UUID(repository["repository_id"]))
        project.run("run", "--rm", "--no-deps", "--pull", "never", "-T", "sync", "--recover",
                    "--repository", identity, stdout=sys.stderr)
    return {"manifest_sha256": expected, "verify_seconds": checked - start,
            "seconds": time.monotonic() - start}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["backup", "verify", "restore"])
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--manifest-sha256")
    parser.add_argument("--project")
    parser.add_argument("--compose-file", type=Path, action="append")
    args = parser.parse_args()
    if args.action != "backup" and not re.fullmatch(r"[0-9a-f]{64}", args.manifest_sha256 or ""):
        parser.error("verify and restore require a trusted --manifest-sha256")
    bundle = args.bundle.resolve()
    os.umask(0o077)
    if args.action == "verify":
        verify(bundle, args.manifest_sha256)
        print(json.dumps({"manifest_sha256": args.manifest_sha256, "verified": True}))
        return
    if not args.project or not args.compose_file or os.getuid() != 0:
        parser.error("backup and restore require root, --project and --compose-file")
    project = Project(args.project, args.compose_file)
    store = project.store()
    result = (backup(project, store, bundle) if args.action == "backup"
              else restore(project, store, bundle, args.manifest_sha256))
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
