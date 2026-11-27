"""Build deterministic native Git upstreams and an independent byte oracle."""

from hashlib import shake_256
import json
import os
from pathlib import Path
import subprocess
from uuid import UUID


HOST = "upstream.phase2.test"
REPOS = {
    "sha1": ("11111111-2222-4333-8444-000000000001", "11111111-2222-4333-8444-000000000011"),
    "sha256": ("11111111-2222-4333-8444-000000000002", "11111111-2222-4333-8444-000000000012"),
}
GIT_ENV = {
    "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_AUTHOR_NAME": "Phase 2", "GIT_AUTHOR_EMAIL": "author@example.invalid",
    "GIT_COMMITTER_NAME": "Phase 2", "GIT_COMMITTER_EMAIL": "author@example.invalid",
    "GIT_AUTHOR_DATE": "2002-02-02T02:02:02Z", "GIT_COMMITTER_DATE": "2002-02-02T02:02:02Z",
}


def run(*args, **kwargs):
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30, **kwargs).stdout


def repo_path(root, fmt, namespace=""):
    return root / "repositories" / namespace / f"{fmt}.git"


def git(repo, *args, data=None):
    return run("git", "--git-dir", repo, *args, input=data, env={**os.environ, **GIT_ENV})


def commit(repo, identity, *, parent=None, size=48):
    body = identity.encode() + b"\r\n\x00\xff" + shake_256(identity.encode()).digest(size)
    blob = git(repo, "hash-object", "-w", "--stdin", data=body).strip()
    tree = git(repo, "mktree", data=b"100644 blob " + blob + b"\toriginal.bin\n").strip()
    parents = ["-p", parent] if parent else []
    value = git(repo, "commit-tree", tree, *parents, "-m", identity).strip().decode()
    return value, blob.decode(), body


def prepare(root, *, host=HOST, namespace=""):
    root.mkdir(parents=True, exist_ok=True)
    certs = root / "tls"
    certs.mkdir()
    run("openssl", "req", "-new", "-x509", "-nodes", "-days", "2", "-newkey", "rsa:2048",
        "-subj", "/CN=Phase 2 synthetic CA", "-keyout", root / "ca.key", "-out", certs / "ca.crt")
    run("openssl", "req", "-new", "-nodes", "-newkey", "rsa:2048", "-subj", f"/CN={host}",
        "-keyout", certs / "upstream.key", "-out", root / "upstream.csr")
    ext = root / "extensions"
    ext.write_text(f"subjectAltName=DNS:{host}\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", root / "upstream.csr", "-CA", certs / "ca.crt",
        "-CAkey", root / "ca.key", "-CAcreateserial", "-out", certs / "upstream.crt", "-days", "2", "-extfile", ext)
    for item in (root / "ca.key", root / "upstream.csr", ext, certs / "ca.srl"):
        item.unlink(missing_ok=True)
    (root / "mode").write_text("normal")
    for fmt in REPOS:
        repo = repo_path(root, fmt, namespace)
        repo.parent.mkdir(parents=True, exist_ok=True)
        run("git", "init", "--bare", f"--object-format={fmt}", "--initial-branch=main", repo)
        initial, blob, raw = commit(repo, f"{fmt}-main", size=384 * 1024)
        optional, _, _ = commit(repo, f"{fmt}-optional")
        secret, _, _ = commit(repo, f"{fmt}-unapproved")
        git(repo, "update-ref", "refs/heads/main", initial)
        git(repo, "update-ref", "refs/heads/approved/release", optional)
        git(repo, "update-ref", "refs/heads/hidden", secret)
        git(repo, "tag", "-a", "v1", initial, "-m", "Approved synthetic tag")
        (root / f"{fmt}-oracle.json").write_text(json.dumps({
            "initial": initial, "blob": blob, "optional": optional, "hidden": secret, "size": len(raw),
        }))
        (root / f"{fmt}-oracle.bin").write_bytes(raw)


def approval(fmt, *, version=1, required=None, deadline=90, response_bytes=8 * 1024 * 1024,
             source=None, repository=None, ref_base=None, pack_bytes=8 * 1024 * 1024, host=HOST):
    repo, source_id = REPOS[fmt]
    return {
        "repository": repository or repo, "source": source or source_id, "format": fmt, "version": version,
        "url": f"https://{host}/{ref_base or fmt}.git", "hosts": [host],
        "required_refs": required or ["refs/heads/main"], "optional_refs": ["refs/tags/v1"],
        "prefixes": ["refs/heads/approved/"], "reviewer": "Synthetic fixture",
        "reviewed_at": "2002-02-02T02:02:02Z", "full_closure_approved": True,
        "output_scope": "Synthetic originals", "license_evidence": "Repository-owned fixture",
        "notice_evidence": "Synthetic author and content",
        "limits": {"pack_bytes": pack_bytes, "response_bytes": response_bytes, "objects": 5000,
                   "deadline_seconds": deadline},
    }


def alternate(base, offset):
    return str(UUID(int=UUID(base).int + offset))
