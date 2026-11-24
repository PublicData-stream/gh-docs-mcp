"""Generate byte-identical bare Git fixtures and a trusted Git CLI oracle."""

import base64
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess


IDENTITIES = (
    ("sha1", "11111111-1111-4111-8111-111111111111",
     "22222222-2222-4222-8222-222222222222", "00000000-0001-7000-8000-000000000001"),
    ("sha256", "33333333-3333-4333-8333-333333333333",
     "44444444-4444-4444-8444-444444444444", "00000000-0001-7000-8000-000000000002"),
)
GIT_ENV = {
    "PATH": os.environ["PATH"],
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_AUTHOR_NAME": "Phase One",
    "GIT_AUTHOR_EMAIL": "phase1@example.invalid",
    "GIT_COMMITTER_NAME": "Phase One",
    "GIT_COMMITTER_EMAIL": "phase1@example.invalid",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
    "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
    "LC_ALL": "C",
    "TZ": "UTC",
}
BARE_CONFIG = {
    "sha1": b"[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
    "sha256": (b"[core]\n\trepositoryformatversion = 1\n\tbare = true\n"
               b"[extensions]\n\tobjectformat = sha256\n"),
}
MAX_BLOB = 8 * 1024 * 1024


def encode(raw):
    return base64.b64encode(raw).decode("ascii")


def write(path, value, mode=0o644):
    path.write_bytes(value)
    path.chmod(mode)


def make_repo(path, object_format, repo, source, job):
    def git(*args, data=None, check=True):
        result = subprocess.run(
            ["git", "-c", f"safe.directory={path}", f"--git-dir={path}", *args],
            input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            check=check, env=GIT_ENV,
        )
        return result.stdout if check else result

    def blob(data):
        return git("hash-object", "-w", "--stdin", data=data).strip()

    def entry(mode, kind, oid, name):
        return b" ".join([mode, kind, oid]) + b"\t" + name + b"\0"

    def commit(message, *parents):
        flags = [arg for parent in parents for arg in ("-p", parent)]
        return git("commit-tree", root, *flags, "-m", message).strip()

    git("init", "--bare", "--template=", "--initial-branch=main",
        f"--object-format={object_format}")
    write(path / "config", BARE_CONFIG[object_format])
    sizes = {"sha1": 40, "sha256": 64}
    binary = blob(b"begin\x00\xff\r\nend\x00")
    empty = blob(b"")
    text = blob("한글\r\nSecond line\r\n".encode())
    symlink = blob(b"../outside")
    subtree = git("mktree", "-z", data=entry(b"100644", b"blob", binary, b"overlap")).strip()
    root = git("mktree", "-z", data=b"".join([
        entry(b"100644", b"blob", binary, b"binary"),
        entry(b"100644", b"blob", binary, b"again"),
        entry(b"100644", b"blob", empty, b"empty"),
        entry(b"100755", b"blob", text, b"line\tbreak\n"),
        entry(b"100644", b"blob", text, b"non-utf8-\xff"),
        entry(b"120000", b"blob", symlink, b"symlink"),
        entry(b"160000", b"commit", b"f" * sizes[object_format], b"submodule"),
        entry(b"040000", b"tree", subtree, b"subtree"),
        entry(b"040000", b"tree", subtree, b"subtree-again"),
    ])).strip()
    base = commit("base")
    old = commit("old tip", base)
    left = commit("left", base)
    right = commit("right", base)
    merge = commit("replacement merge", left, right)
    git("update-ref", "refs/heads/main", old)
    git("update-ref", "refs/heads/pack-orphan", old)
    git("update-ref", "refs/heads/main", merge, old)
    git("update-ref", b"refs/heads/bytes-\xff", right)
    git("tag", "-a", "release", "-m", "annotated commit tag", left)
    git("tag", "-a", "blob", "-m", "annotated blob tag", binary)
    git("repack", "-a", "-d", "-n")
    git("update-ref", "-d", "refs/heads/pack-orphan")
    oversized = blob(b"x" * (MAX_BLOB + 1))
    assert list((path / "objects/pack").glob("*.pack"))
    assert (path / "objects" / os.fsdecode(oversized[:2]) / os.fsdecode(oversized[2:])).is_file()
    assert git("merge-base", "--is-ancestor", left, merge, check=False).returncode == 0
    assert git("merge-base", "--is-ancestor", old, merge, check=False).returncode == 1
    git("fsck", "--strict")
    (path / "info").mkdir(exist_ok=True)
    for parent, _, files in os.walk(path):
        parent = Path(parent)
        parent.chmod(0o755)
        for name in files:
            entry_path = parent / name
            entry_path.chmod(stat.S_IMODE(entry_path.stat().st_mode) & ~0o7022)
    entries = []
    for record in git("ls-tree", "-z", root).split(b"\0")[:-1]:
        header, name = record.split(b"\t", 1)
        mode, kind, oid = header.split()
        entries.append({
            "name": encode(name),
            "mode": mode.decode(),
            "oid": oid.decode(),
            "bytes": encode(git("cat-file", "blob", oid)) if kind == b"blob" else None,
        })
    refs = []
    for name in git("for-each-ref", "--format=%(refname)").splitlines():
        oid = git("rev-parse", "--verify", name).strip()
        kind = git("cat-file", "-t", oid).strip()
        peeled = git("rev-parse", "--verify", oid + b"^{commit}", check=False)
        refs.append({
            "name": encode(name),
            "target": oid.decode(),
            "kind": kind.decode(),
            "peeled": peeled.stdout.strip().decode() if peeled.returncode == 0 else None,
        })
    return {
        "repo": repo, "source": source, "job": job, "format": object_format,
        "root": root.decode(), "old": old.decode(), "left": left.decode(), "merge": merge.decode(),
        "tag": git("rev-parse", "refs/tags/release").strip().decode(),
        "noncommit_tag": git("rev-parse", "refs/tags/blob").strip().decode(),
        "oversized": oversized.decode(), "entries": entries, "refs": refs,
    }


def generate(store, oracle, incoming=None):
    repos = store / "repositories"
    repos.mkdir(parents=True, exist_ok=True)
    store.chmod(0o755)
    repos.chmod(0o755)
    records = []
    for object_format, repo, source, job in IDENTITIES:
        record = make_repo(repos / repo, object_format, repo, source, job)
        if incoming is not None:
            incoming.mkdir(mode=0o700, parents=True, exist_ok=True)
            shutil.copytree(repos / repo, incoming / job)
        records.append(record)
    write(oracle, json.dumps(records, separators=(",", ":"), sort_keys=True).encode())
    return records


def own_tree(path):
    """Use the real sync UID/GID on the rootful Compose profile."""
    for parent, directories, files in os.walk(path):
        os.chown(parent, 10002, 10002)
        for entry in [*directories, *files]:
            os.chown(Path(parent) / entry, 10002, 10002, follow_symlinks=False)
