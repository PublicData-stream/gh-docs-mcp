"""Qualify private exploration in the rootful Compose trust boundaries."""

import argparse
from base64 import b64decode
from datetime import datetime, timezone
import importlib.util
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase2"))
from smart_fixtures import REPOS, approval, git, repo_path
from object_fixtures import CROSS_REPO, CROSS_SOURCE, NOTICE, PATH_NOTICE, UTF8, decoded, encoded, oracle, prepare, reviewed

spec = importlib.util.spec_from_file_location("phase3_integration", ROOT / "scripts/phase3/integration.py")
phase3 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase3)
phase2 = phase3.phase2

DRIVER = "gh-docs-phase4-fixture"
SCHEMA = ROOT / "scripts/phase4/request.schema.json"


def call(stack, tool, arguments=None, *, error=None, fail_setup=False):
    request = {"tool": tool, "arguments": arguments} if isinstance(tool, str) else tool
    result = stack.compose(
        "exec", "-T", "-e", "EXPLORATION_CURSOR_KEY_FILE=/run/secrets/cursor.key",
        "-e", "EXPLORATION_CURSOR_KEY_ID=fixture-v1", "serve", DRIVER,
        *(["--fail-setup"] if fail_setup else []), input=json.dumps(request), timeout=35,
    )
    assert len(result.stdout.encode()) <= (16 if isinstance(tool, list) else 1) * (256 * 1024 + 32)
    response = json.loads(result.stdout)
    if isinstance(tool, list):
        return response
    if error:
        assert response["error"]["code"] == error, response
        return response
    assert "result" in response, response
    return response["result"]


def context(stack, repo, selector=None, **kwargs):
    args = {"repository_id": repo, "selector": selector or {"kind": "default"}, **kwargs}
    return call(stack, "git.resolve", args)["context"]


def ref(name):
    return {"kind": "ref", "value": encoded(name)}


def by_oid(oid):
    return {"kind": "oid", "value": oid}


def read(stack, ctx, path, **kwargs):
    return call(stack, "git.read", {"context": ctx, "path": encoded(path), **kwargs})


def stage(stack, config, endpoint, fmt, *, repository=None, **options):
    policy = reviewed(fmt, repository=repository, **options)
    phase2.register(stack, config, policy)
    generation = phase2.fetch(stack, repository or policy["repository"], endpoint)
    return int(generation["generation"])


def prepare_publish(stack, repo, generation, seq=0):
    phase3.command(stack, repo, "--prepare", generation)
    phase3.publish(stack, repo, generation, seq)


def schema_and_grants(stack):
    schema = json.loads(stack.exec("serve", DRIVER, "--schema").stdout)
    assert schema == json.loads(SCHEMA.read_text())
    assert set(schema["$defs"]["Context"]["required"]) == {
        "repository_id", "generation_id", "object_format", "commit_oid",
    }
    assert schema["$defs"]["Context"]["additionalProperties"] is False
    assert phase2.sql(stack, """
        SELECT has_table_privilege('gh_docs_serve', 'git_catalog.exploration_generations', 'SELECT'),
               has_table_privilege('gh_docs_serve', 'git_catalog.exploration_generations', 'UPDATE'),
               has_table_privilege('gh_docs_serve', 'git_catalog.source_approvals', 'SELECT'),
               has_table_privilege('gh_docs_sync', 'git_catalog.exploration_generations', 'SELECT')
    """) == "t|f|f|f"


def migration(stack, repo, expected):
    # Restore the Phase 3 schema on this disposable database with published data.
    phase2.sql(stack, """BEGIN; DROP VIEW git_catalog.exploration_generations;
        DELETE FROM git_catalog.migrations WHERE version IN (5, 6); COMMIT;""")
    assert phase2.sql(stack, "SELECT version FROM git_catalog.migrations ORDER BY version") == "2\n3\n4"
    stack.one_shot("migrate")
    stack.one_shot("migrate")
    assert call(stack, "repository.info", {"repository_id": repo}) == expected


def page_all(stack, tool, arguments, *, limit=2):
    entries = []
    cursor = None
    while True:
        page = call(stack, tool, {**arguments, "page": {"limit": limit, "cursor": cursor}})
        entries.extend(page["entries"])
        following = page["next_cursor"]
        assert page["coverage"]["complete"] == (following is None), page["coverage"]
        if not following:
            return entries
        assert following != cursor and page["entries"], page["coverage"]
        cursor = following


def object_oracle(stack, upstream, fmt, ctx):
    repo = repo_path(upstream, fmt)
    reference = oracle(repo, ctx["commit_oid"])
    allowed = sorted(reference) if fmt == "sha1" else sorted(
        name for name in reference if name in {b"data", b"dir", b"dir/allowed", b"original.bin"}
        or name.startswith(b"data/"))
    entries = page_all(stack, "git.ls_tree", {"context": ctx, "depth": 4})
    names = [decoded(entry["path"]) for entry in entries]
    assert names == allowed, names
    for name, entry in zip(names, entries, strict=True):
        mode, oid = reference[name]
        if fmt == "sha256" and name == b"dir":
            assert entry["navigation_only"] and "oid" not in entry and entry["notice"] == NOTICE
        else:
            assert (entry["mode"], entry["oid"]) == (mode, oid), entry
            assert not entry["navigation_only"] and entry["notice"] == PATH_NOTICE
        if mode == "040000":
            continue
        if mode == "160000":
            output = read(stack, ctx, name)
            assert output["kind"] == "gitlink" and output["oid"] == oid and "data" not in output
            continue
        raw = git(repo, "cat-file", "blob", oid)
        output = read(stack, ctx, name, encoding="base64")
        actual = bytearray()
        while True:
            current = output["returned_range"]
            data = b64decode(output["data"], validate=True)
            assert len(data) <= 32768
            assert current["start"] == len(actual)
            assert current["end"] == len(actual) + len(data)
            assert output["total_bytes"] == len(raw) and output["oid"] == oid
            assert output["notice"] == PATH_NOTICE and output["context"] == ctx
            actual.extend(data)
            if output["next_cursor"] is None:
                assert output["coverage"]["complete"]
                break
            output = read(stack, ctx, name, encoding="base64", cursor=output["next_cursor"])
        assert bytes(actual) == raw, name

    utf8 = read(stack, ctx, "data/utf8")
    assert utf8["returned_range"] == {"start": 0, "end": 32767}
    assert read(stack, ctx, "data/utf8", cursor=utf8["next_cursor"])["data"].encode() == UTF8[32767:]
    assert read(stack, ctx, "data/utf8", range={"start": 32767, "end": 32770})["data"] == "€"
    for start, end, code in [(32768, 32771, "invalid_utf8_boundary"), (0, 32769, "invalid_utf8_boundary"),
                             (0, 100_000, "invalid_argument"), (3, 2, "invalid_argument")]:
        call(stack, "git.read", {"context": ctx, "path": encoded("data/utf8"),
                                "range": {"start": start, "end": end}}, error=code)
    call(stack, "git.read", {"context": ctx, "path": encoded("data/binary")}, error="unsupported_encoding")
    assert read(stack, ctx, "data/empty")["data"] == ""
    assert call(stack, "git.ls_tree", {"context": ctx, "root": encoded("data/empty-tree")})["entries"] == []
    assert [decoded(entry["path"]) for entry in page_all(stack, "git.find", {
        "context": ctx, "pattern": {"kind": "glob", "value": encoded("**/invalid-?")},
    })] == [b"data/invalid-\xff"]
    assert [decoded(entry["path"]) for entry in page_all(stack, "git.find", {
        "context": ctx, "pattern": {"kind": "literal", "value": encoded(":(glob)*")},
    })] == [b"data/:(glob)*"]
    for invalid in ("../data/binary", "data//binary", "data/./binary", "/git-store", "data/binary\0"):
        call(stack, "git.read", {"context": ctx, "path": encoded(invalid)}, error="invalid_argument")
    for missing in ("data/absent", "data/link/secret", "data/submodule/file"):
        call(stack, "git.read", {"context": ctx, "path": encoded(missing)}, error="object_unavailable")
    if fmt == "sha256":
        hidden = [call(stack, "git.read", {"context": ctx, "path": encoded(path)}, error="object_unavailable")
                  for path in ("dir/secret", "dir/does-not-exist", "missing-parent/hidden")]
        assert hidden[0] == hidden[1] == hidden[2]
        call(stack, "git.ls_tree", {"context": ctx, "root": encoded("missing-parent")},
             error="object_unavailable")


def resolve_boundaries(stack, upstream, fmt, repo, generation):
    proof = json.loads((upstream / f"{fmt}-oracle.json").read_text())
    ctx = context(stack, repo)
    assert ctx == {"repository_id": repo, "generation_id": str(generation),
                   "commit_oid": proof["initial"], "object_format": fmt}
    assert context(stack, repo, ref("refs/tags/v2")) == ctx
    assert context(stack, repo, by_oid(proof["tag_oid"])) == ctx
    assert context(stack, repo, by_oid(proof["parent"]))["commit_oid"] == proof["parent"]
    refs = page_all(stack, "git.refs", {"repository_id": repo}, limit=1)
    assert [decoded(entry["name"]) for entry in refs] == sorted(decoded(entry["name"]) for entry in refs)
    assert next(r["peeled_commit"] for r in refs if decoded(r["name"]) == b"refs/tags/v2") == proof["initial"]
    unknown = "0" * len(proof["initial"])
    denied = [call(stack, "git.resolve", {"repository_id": repo, "selector": selector}, error="object_unavailable")
              for selector in [ref("refs/heads/hidden"), ref(f"refs/gh-docs/retention/{generation}/0"),
                               ref("refs/tags/hidden-noncommit"), by_oid(unknown),
                               by_oid(proof["hidden"]), by_oid(proof["hidden_tag"]), by_oid(proof["blob"])]]
    assert all(response == denied[0] for response in denied)
    for value in ("HEAD~1", "refs/heads/main@{1}", "refs/heads/main:original.bin", ":/anything"):
        call(stack, "git.resolve", {"repository_id": repo, "selector": ref(value)}, error="invalid_argument")
    mutated = {**ctx, "commit_oid": proof["hidden"]}
    call(stack, "git.ls_tree", {"context": mutated}, error="object_unavailable")
    call(stack, "git.read", {"context": {**ctx, "generation_id": "00"}, "path": encoded("original.bin")},
         error="invalid_argument")
    call(stack, "git.ls_tree", {"context": {**ctx, "publication_sequence": "1"}}, error="invalid_argument")
    return ctx


def paging_and_policy(stack, upstream, config, endpoint, ctx, restricted):
    repo = ctx["repository_id"]
    first = int(ctx["generation_id"])
    args = {"repository_id": repo, "page": {"limit": 1}}
    first_page = call(stack, "git.refs", args)
    token = first_page["next_cursor"]
    tree_args = {"context": ctx, "depth": 4, "page": {"limit": 1}}
    old_tree = call(stack, "git.ls_tree", tree_args)
    tree_token = old_tree["next_cursor"]
    invalid = "A" + token[1:] if token[0] != "A" else "B" + token[1:]
    call(stack, "git.refs", {**args, "page": {"limit": 1, "cursor": invalid}}, error="cursor_invalid")
    call(stack, "git.refs", {**args, "page": {"limit": 2, "cursor": token}}, error="cursor_invalid")
    call(stack, "git.find", {"context": ctx, "pattern": {"kind": "literal", "value": encoded("data")},
                             "page": {"limit": 1, "cursor": tree_token}}, error="cursor_invalid")
    origin = repo_path(upstream, "sha1")
    for reference in ("refs/heads/approved/large", "refs/heads/approved/release", "refs/tags/v1", "refs/tags/v2"):
        git(origin, "update-ref", "-d", reference)
    second, new_oid, _, _ = phase3.new_generation(stack, upstream, repo, endpoint, "Phase 4 force push")
    coordinates = {"repository_id": repo, "generation_id": str(second), "selector": by_oid(new_oid)}
    call(stack, "git.resolve", coordinates, error="generation_unavailable")
    phase3.command(stack, repo, "--prepare", second)
    call(stack, "git.resolve", coordinates, error="generation_unavailable")
    phase3.publish(stack, repo, second, 1)
    phase3.pack_read(stack, repo, ctx["commit_oid"])
    call(stack, "git.resolve", {"repository_id": repo, "selector": by_oid(ctx["commit_oid"])},
         error="object_unavailable")
    resume_refs = {**args, "page": {"limit": 1, "cursor": token}}
    resume_tree = {**tree_args, "page": {"limit": 1, "cursor": tree_token}}
    old_refs = call(stack, "git.refs", resume_refs)
    old_result = call(stack, "git.ls_tree", resume_tree)
    assert old_refs["source"]["generation_id"] == str(first)
    assert old_result["context"] == ctx
    phase3.command(stack, repo, "--rollback", first, seq=2)
    for tool, args, expected in (("git.refs", resume_refs, old_refs), ("git.ls_tree", resume_tree, old_result)):
        actual = call(stack, tool, args)
        assert actual == expected, actual
    assert context(stack, repo) == ctx

    phase3.command(stack, repo, "--rollback", second, seq=3)
    parent = new_oid
    for seq in range(4, 8):
        generation, parent, _, _ = phase3.new_generation(stack, upstream, repo, endpoint,
                                                         f"Phase 4 retention {seq}", parent=parent)
        prepare_publish(stack, repo, generation, seq)
    phase2.sql(stack, f"""UPDATE git_catalog.repository_generations
        SET demoted_at = clock_timestamp() - interval '25 hours'
        WHERE repository_id = '{repo}' AND generation_id = {first}""")
    phase3.command(stack, repo, "--gc", fault="after_retire")
    assert phase3.lifecycle(stack, repo, first) == "retired"
    phase3.pack_read(stack, repo, ctx["commit_oid"])
    call(stack, "git.ls_tree", resume_tree, error="generation_unavailable")
    call(stack, "git.refs", resume_refs, error="generation_unavailable")
    phase3.command(stack, repo, "--recover")

    restricted_repo = restricted["repository_id"]
    restricted_page = call(stack, "git.ls_tree", {"context": restricted, "page": {"limit": 1}})
    phase2.register(stack, config, reviewed("sha256", version=2))
    call(stack, "git.ls_tree", {"context": restricted, "page": {
        "limit": 1, "cursor": restricted_page["next_cursor"],
    }}, error="generation_unavailable")
    generation = int(phase2.fetch(stack, restricted_repo, endpoint)["generation"])
    prepare_publish(stack, restricted_repo, generation, 1)
    renewed = context(stack, restricted_repo)
    assert renewed["generation_id"] == str(generation)
    phase2.sql(stack, f"""SET ROLE gh_docs_migrate;
        SELECT git_catalog.revoke_source('{REPOS["sha256"][1]}')""")
    call(stack, "repository.info", {"repository_id": restricted_repo}, error="repository_unavailable")
    call(stack, "git.read", {"context": renewed, "path": encoded("data/utf8")}, error="generation_unavailable")
    print("A13–A14 pinned pages, rollback, retirement, supersession and revocation passed", flush=True)


def main():
    args = argparse.ArgumentParser(description=__doc__)
    args.add_argument("--image-tag", required=True)
    tag = args.parse_args().image_tag
    print(f"{datetime.now(timezone.utc).isoformat()} Phase 4 Compose qualification", flush=True)
    with phase2.stack(tag, publication=True, fixture_setup=prepare, serve_target="phase4-serve") as (
        stack, upstream, config, endpoint,
    ):
        schema_and_grants(stack)
        assert call(stack, "repository.search", {})["entries"] == []
        # Prior approvals can still publish through Phase 3 without granting output.
        legacy = approval("sha1", repository=CROSS_REPO, source=CROSS_SOURCE, ref_base="cross")
        phase2.register(stack, config, legacy)
        old = int(phase2.fetch(stack, CROSS_REPO, endpoint)["generation"])
        prepare_publish(stack, CROSS_REPO, old)
        call(stack, "repository.info", {"repository_id": CROSS_REPO}, error="repository_unavailable")

        contexts = []
        for fmt, (repo, _) in REPOS.items():
            generation = stage(stack, config, endpoint, fmt)
            prepare_publish(stack, repo, generation)
            ctx = resolve_boundaries(stack, upstream, fmt, repo, generation)
            object_oracle(stack, upstream, fmt, ctx)
            contexts.append(ctx)
        sha1, sha256 = contexts
        metadata = call(stack, "repository.info", {"repository_id": sha1["repository_id"]})
        assert metadata["metadata_notice"] == NOTICE and metadata["source_identity"].endswith("/sha1.git")
        migration(stack, sha1["repository_id"], metadata)
        generation = stage(stack, config, endpoint, "sha1", repository=CROSS_REPO, source=CROSS_SOURCE,
                           ref_base="cross", version=2)
        prepare_publish(stack, CROSS_REPO, generation, 1)
        call(stack, "git.resolve", {"repository_id": CROSS_REPO, "selector": by_oid(sha1["commit_oid"])},
             error="object_unavailable")
        assert [row["repository_id"] for row in page_all(stack, "repository.search", {}, limit=1)] == [
            sha1["repository_id"], sha256["repository_id"], CROSS_REPO,
        ]
        assert [row["repository_id"] for row in page_all(stack, "repository.search", {"query": "sha1.git"})] == [
            sha1["repository_id"],
        ]
        print("A06 and A12 byte oracles, policy projection, migration and object authorization passed", flush=True)

        large = context(stack, sha1["repository_id"], ref("refs/heads/approved/large"))
        partial = call(stack, "git.find", {"context": large, "root": encoded("many"),
                                           "pattern": {"kind": "literal", "value": encoded("absent")}})
        assert partial["coverage"]["reason"] == "narrow_scope" and not partial["coverage"]["complete"]
        assert partial["entries"] == [] and partial["next_cursor"] is None
        call(stack, "git.read", {"context": large, "path": encoded("oversized"), "encoding": "base64"},
             error="budget_exceeded")
        call(stack, "git.ls_tree", {"context": sha1}, fail_setup=True, error="sandbox_unavailable")
        simultaneous = [{"tool": "git.ls_tree", "arguments": {"context": sha1}} for _ in range(16)]
        results = call(stack, simultaneous)
        assert any("result" in result for result in results)
        assert any(result.get("error", {}).get("code") == "budget_exceeded" for result in results)
        assert all("result" in result or result["error"]["code"] == "budget_exceeded" for result in results)
        paging_and_policy(stack, upstream, config, endpoint, sha1, sha256)
    print("Phase 4 Compose qualification passed", flush=True)


if __name__ == "__main__":
    main()
