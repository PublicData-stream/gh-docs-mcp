"""Deterministic merge, text, binary and selective history fixtures."""

import json

from object_fixtures import dir_entry, file_entry, reviewed, tree
from smart_fixtures import REPOS, git, prepare as prepare_upstream, repo_path

PHRASE = "資料 match"
PATH = b"data/note"
OLD = ("old\r\n" + PHRASE + "\r\nlast").encode()
NEW = ("new\r\n" + PHRASE + "\r\nlast").encode()


def policy(fmt, version=1, *, enabled=True):
    result = reviewed(fmt, version=version)
    if enabled:
        result["exploration"].update(version=2, outputs={"search": True, "history": True})
    result["limits"]["objects"] = 5000
    return result


def prepare(root):
    prepare_upstream(root)
    for fmt in REPOS:
        repo = repo_path(root, fmt)
        proof_path = root / f"{fmt}-oracle.json"
        proof = json.loads(proof_path.read_text())

        def contents(raw, hidden):
            return tree(repo, [
                dir_entry(repo, b"data", [
                    file_entry(repo, b"note", raw),
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
        git(repo, "update-ref", "refs/heads/main", head)
        git(repo, "tag", "-fa", "v1", head, "-m", "Phase 5 source")
        proof.update(base=base, left=left, right=right, head=head)
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
            proof.update(partial=partial, slow=slow)
        proof_path.write_text(json.dumps(proof))
