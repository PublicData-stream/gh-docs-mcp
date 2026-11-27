"""Deterministic merge, text, binary and selective history fixtures."""

import json

from object_fixtures import dir_entry, file_entry, reviewed, tree
from smart_fixtures import REPOS, git, prepare as prepare_upstream, repo_path

PHRASE = "資料 match"
PATH = b"data/note"
OLD = ("old\r\n" + PHRASE + "\r\nlast").encode()
NEW = ("new\r\n" + PHRASE + "\r\nlast").encode()
GLOB_REF = "refs/heads/approved/glob"
GLOB_TEXT = b"markdown fixture needle\n"


def policy(fmt, version=1, *, enabled=True):
    result = reviewed(fmt, version=version)
    if enabled:
        result["exploration"].update(version=2, outputs={"search": True, "history": True})
    result["limits"]["objects"] = 5000
    return result


def glob_policy(fmt, version=1):
    result = policy(fmt, version)
    result["exploration"].update(version=3, scope={"kind": "paths", "grants": [
        {"glob": "docs/**/*.md", "notice": "Reviewed Markdown"},
        {"glob": "docs/setup/*.md", "notice": "Secondary notice"},
        {"path": {"encoding": "utf8", "value": "docs/guide.md"},
         "subtree": False, "notice": "Reviewed guide"},
        {"path": {"encoding": "utf8", "value": "LICENSE"},
         "subtree": False, "notice": "Reviewed license"},
    ]})
    return result


def glob_revisions(repo, revision, gitlink):
    def contents(hidden, moved=False, added=False):
        docs = [
            file_entry(repo, b"guide.md", GLOB_TEXT),
            file_entry(repo, b"raw-\xff.md", GLOB_TEXT),
            file_entry(repo, b"shared.txt", GLOB_TEXT),
            file_entry(repo, b"upper.MD", GLOB_TEXT),
            file_entry(repo, b"link.md", b"../hidden.md", b"120000"),
            (b"160000", b"commit", gitlink.encode(), b"submodule.md"),
            dir_entry(repo, b"excluded", [file_entry(repo, b"notes.txt", hidden)]),
            dir_entry(repo, b"setup", [file_entry(repo, b"install.md", GLOB_TEXT, b"100755")]),
        ]
        root = [file_entry(repo, b"LICENSE", b"Synthetic license\n"),
                file_entry(repo, b"hidden.md", GLOB_TEXT)]
        if moved:
            root.append(file_entry(repo, b"moved.md", GLOB_TEXT))
            docs.append(file_entry(repo, b"mode.md", GLOB_TEXT))
        else:
            docs.append(file_entry(repo, b"rename.md", GLOB_TEXT))
            docs.append(file_entry(repo, b"mode.md", b"../hidden.md", b"120000"))
        if added:
            docs.append(file_entry(repo, b"future.md", GLOB_TEXT))
        return tree(repo, [*root, dir_entry(repo, b"docs", docs)])

    base = revision(contents(b"before"))
    hidden = revision(contents(b"after"), base)
    head = revision(contents(b"after", moved=True), hidden)
    future = revision(contents(b"after", moved=True, added=True), head)
    git(repo, "update-ref", GLOB_REF, head)
    nested = file_entry(repo, b"nonmatching.txt", b"outside scope")
    for _ in range(63):
        nested = dir_entry(repo, b"deep", [nested])
    budget = revision(tree(repo, [dir_entry(repo, b"docs", [nested])]))
    git(repo, "update-ref", "refs/heads/approved/glob-budget", budget)
    return {"base": base, "hidden": hidden, "head": head, "future": future, "budget": budget}


def prepare(root, *, extra_tls_names=()):
    prepare_upstream(root, extra_tls_names=extra_tls_names)
    for fmt in REPOS:
        repo = repo_path(root, fmt)
        proof_path = root / f"{fmt}-oracle.json"
        proof = json.loads(proof_path.read_text())

        def contents(raw, hidden, note=b"note"):
            return tree(repo, [
                dir_entry(repo, b"data", [
                    file_entry(repo, note, raw),
                    file_entry(repo, b"same-\xff", raw),
                    file_entry(repo, b"binary", b"\0" + PHRASE.encode() + bytes([hidden])),
                    file_entry(repo, b":(glob)*", b"literal path\r\n"),
                    file_entry(repo, b"wide", b"\x01" * 32768),
                ]),
                file_entry(repo, b".gitattributes", b"* diff=sentinel\n"),
                file_entry(repo, b"private", f"hidden fixture {hidden}".encode()),
            ])

        def revision(files, *parents):
            parent_flags = [flag for oid in parents for flag in ("-p", oid)]
            return git(repo, "commit-tree", files, *parent_flags, "-m", "omitted synthetic message").strip().decode()

        base = revision(contents(OLD, 1))
        left = revision(contents(NEW, 1), base)
        right = revision(contents(OLD, 2), base)
        head = revision(contents(NEW, 2), left, right)
        renamed = revision(contents(NEW, 2, b"renamed"), head)
        git(repo, "update-ref", "refs/heads/approved/rename", renamed)
        git(repo, "update-ref", "refs/heads/main", head)
        git(repo, "tag", "-fa", "v1", head, "-m", "Phase 5 source")
        proof.update(base=base, left=left, right=right, head=head, renamed=renamed)
        proof["glob"] = glob_revisions(repo, revision, proof["hidden"])
        if fmt == "sha1":
            corpus = [file_entry(repo, f"{i:03d}".encode(), b"nonmatching line\n") for i in range(34)]
            corpus.append(file_entry(repo, b"last", b"needle\n"))
            partial = revision(tree(repo, [dir_entry(repo, b"corpus", corpus)]))
            git(repo, "update-ref", "refs/heads/approved/partial", partial)
            # Compression keeps the upstream test small while the search worker
            # receives several megabytes of approved bytes per candidate.
            large = b"abcdefghij012345\n" * 300_000
            stress = [file_entry(repo, f"{i:03d}".encode(), large) for i in range(32)]
            slow = revision(tree(repo, [dir_entry(repo, b"stress", stress)]))
            git(repo, "update-ref", "refs/heads/approved/stress", slow)
            oversized = revision(tree(repo, [file_entry(repo, b"oversized", b"x" * (8 * 1024 * 1024 + 1))]))
            git(repo, "update-ref", "refs/heads/approved/oversized", oversized)
            proof.update(partial=partial, slow=slow, oversized=oversized)
        proof_path.write_text(json.dumps(proof))
