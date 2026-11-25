"""Synthetic bare Git trees and approved disclosure policies."""

from base64 import b64decode, b64encode
import json

from smart_fixtures import approval, commit, git, prepare as prepare_upstream, repo_path, run

CROSS_REPO = "11111111-2222-4333-8444-000000000003"
CROSS_SOURCE = "11111111-2222-4333-8444-000000000013"
UTF8 = b"x" * 32767 + "€".encode() + b"\r\n\x00end"
NOTICE = "Synthetic metadata notice"
PATH_NOTICE = "Synthetic reviewed path bytes"


def encoded(raw):
    if isinstance(raw, str):
        return {"encoding": "utf8", "value": raw}
    try:
        return {"encoding": "utf8", "value": raw.decode()}
    except UnicodeDecodeError:
        return {"encoding": "base64", "value": b64encode(raw).decode()}


def decoded(value):
    return value["value"].encode() if value["encoding"] == "utf8" else b64decode(value["value"], validate=True)


def reviewed(fmt, *, version=1, whole=None, **kwargs):
    if whole is None:
        whole = fmt == "sha1"
    selected = approval(fmt, version=version, **kwargs)
    selected["optional_refs"].append("refs/tags/v2")
    if whole:
        scope = {"kind": "whole", "notice": PATH_NOTICE}
    else:
        scope = {"kind": "paths", "grants": [
            {"path": encoded("data"), "subtree": True, "notice": PATH_NOTICE},
            {"path": encoded("dir/allowed"), "subtree": False, "notice": PATH_NOTICE},
            {"path": encoded("original.bin"), "subtree": False, "notice": PATH_NOTICE},
            {"path": encoded("missing-parent/absent"), "subtree": False, "notice": PATH_NOTICE},
        ]}
    selected["exploration"] = {"version": 1, "metadata_notice": NOTICE, "scope": scope}
    return selected


def blob(repo, raw):
    return git(repo, "hash-object", "-w", "--stdin", data=raw).strip()


def tree(repo, entries):
    payload = b"".join(mode + b" " + kind + b" " + oid + b"\t" + name + b"\0"
                       for mode, kind, oid, name in entries)
    return git(repo, "mktree", "-z", "--missing", data=payload).strip()


def file_entry(repo, name, contents, mode=b"100644"):
    return mode, b"blob", blob(repo, contents), name


def dir_entry(repo, name, entries):
    return b"040000", b"tree", tree(repo, entries), name


def prepare(root):
    prepare_upstream(root)
    for fmt in ("sha1", "sha256"):
        path = root / f"{fmt}-oracle.json"
        proof = json.loads(path.read_text())
        repo = repo_path(root, fmt)
        original = (root / f"{fmt}-oracle.bin").read_bytes()
        regular = file_entry(repo, b"original.bin", original)
        data = dir_entry(repo, b"data", [
            file_entry(repo, b"binary", b"\r\n\x00\xff"),
            file_entry(repo, b"invalid-\xff", b"raw name"),
            file_entry(repo, b"empty", b""),
            file_entry(repo, b"utf8", UTF8),
            file_entry(repo, b"-flag", b"literal dash", b"100755"),
            file_entry(repo, b":(glob)*", b"literal metacharacters"),
            file_entry(repo, b"link", b"../dir/secret", b"120000"),
            (b"160000", b"commit", proof["hidden"].encode(), b"submodule"),
            dir_entry(repo, b"empty-tree", []),
        ])
        nested = dir_entry(repo, b"dir", [
            file_entry(repo, b"allowed", b"reviewed leaf"),
            file_entry(repo, b"secret", b"unreviewed bytes"),
        ])
        absent = dir_entry(repo, b"missing-parent", [file_entry(repo, b"hidden", b"hidden sibling")])
        root_tree = tree(repo, [regular, data, nested, absent, file_entry(repo, b"dir-", b"sort")])
        main = git(repo, "commit-tree", root_tree, "-p", proof["initial"], "-m", "Object exploration").strip()
        git(repo, "update-ref", "refs/heads/main", main)
        git(repo, "tag", "-fa", "v1", main, "-m", "Approved tag")
        git(repo, "tag", "-a", "v2", "refs/tags/v1", "-m", "Nested approved tag")
        proof["parent"] = proof["initial"]
        proof["initial"] = main.decode()
        proof["tag_oid"] = git(repo, "rev-parse", "refs/tags/v2").strip().decode()
        git(repo, "tag", "-a", "hidden-noncommit", regular[2], "-m", "Unapproved noncommit tag")
        proof["hidden_tag"] = git(repo, "rev-parse", "refs/tags/hidden-noncommit").strip().decode()
        if fmt == "sha1":
            oversized = b"repeat\n" * 1_200_000
            repeated = blob(repo, b"bounded tree fixture")
            entries = [(b"100644", b"blob", repeated, f"{index:06}".encode()) for index in range(50_000)]
            large = tree(repo, [dir_entry(repo, b"many", entries),
                                file_entry(repo, b"oversized", oversized)])
            target = git(repo, "commit-tree", large, "-m", "Budget fixture commit").strip()
            git(repo, "update-ref", "refs/heads/approved/large", target)
        path.write_text(json.dumps(proof))

    repo = repo_path(root, "cross")
    run("git", "init", "--bare", "--object-format=sha1", "--initial-branch=main", repo)
    oid, _, _ = commit(repo, "independent SHA-1 repository")
    git(repo, "update-ref", "refs/heads/main", oid)


def oracle(repo, oid):
    output = git(repo, "ls-tree", "-rtz", oid)
    result = {}
    for line in output.split(b"\0")[:-1]:
        header, name = line.split(b"\t", 1)
        mode, _, target = header.decode().split(" ")
        result[name] = (mode, target)
    return result
