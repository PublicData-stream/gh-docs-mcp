#!/usr/bin/env python3
"""Rehearse the docs-mcp rename in place with populated synthetic state."""

import argparse
from base64 import b64decode
import importlib.util
import json
import os
from pathlib import Path

import backup


spec = importlib.util.spec_from_file_location("phase8_integration", Path(__file__).with_name("integration.py"))
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
phase2, phase3, phase5 = fixtures.phase2, fixtures.phase3, fixtures.phase5


def use_previous(stack, override, source, tag):
    current = stack.compose_command.copy(), stack.context.copy()
    base = source / "compose.yml"
    overlay = stack.root / "previous.json"
    config = json.loads(override.read_text())
    roles = config["services"]
    for role in ("serve", "sync", "feed", "postgres"):
        settings = roles.setdefault(role, {})
        settings["image"] = f"gh-docs-mcp-{role}:{tag}"
        if "environment" in settings:
            settings["environment"] = {
                key.replace("DOCS_MCP_", "GH_DOCS_", 1) if key.startswith("DOCS_MCP_") else key: value
                for key, value in settings["environment"].items()
            }
    roles["migrate"]["image"] = roles["sync"]["image"]
    overlay.write_text(json.dumps(config))
    old_files = {str(phase2.ROOT / "compose.yml"): str(base), str(override): str(overlay)}
    stack.compose_command = [old_files.get(arg, arg) for arg in stack.compose_command]
    stack.context.pop("DOCS_MCP_IMAGE_TAG")
    stack.context["GH_DOCS_IMAGE_TAG"] = tag
    return current, (base, overlay)


def quiesce(stack):
    stack.compose("stop", "proxy")
    stack.compose("stop", "feed")
    stack.compose("stop", "sync", "serve")


def start_postgres(stack, configuration):
    stack.compose("stop", "postgres")
    stack.compose_command, stack.context = configuration
    up = ("up", "-d", "--no-build", "--pull", "never")
    stack.compose(*up, "--wait", "--wait-timeout", "130", "postgres", timeout=145)


def start_roles(stack):
    up = ("up", "-d", "--no-build", "--pull", "never")
    stack.compose("run", "--rm", "--no-deps", "migrate", "--migrate")
    stack.compose(*up, "serve", "sync", "feed", timeout=90)
    for role in ("serve", "sync", "feed"):
        phase3.wait_until(lambda: stack.exec(
            role, "wget", "-T", "2", "-qO", "/dev/null", "http://127.0.0.1:8081/healthz",
            expected=None,
        ).returncode == 0, f"{role} did not become ready", seconds=25)
    stack.compose(*up, "proxy")


def restore_previous(stack, configuration, project, bundle, digest):
    stack.compose_command, stack.context = configuration
    refused = stack.compose("run", "--rm", "--no-deps", "migrate", "--migrate", expected=None)
    assert refused.returncode and "unsupported Git catalog migration version" in refused.stdout
    stack.compose("stop", "postgres")
    # Preserve the upgraded volumes until the disposable rehearsal exits.
    for name in ("store", "staging", "pgdata"):
        volume = stack.root / name
        volume.rename(stack.root / f"upgraded-{name}")
        volume.mkdir()
    stack.compose("run", "--rm", "--no-deps", "volume-admin")
    start_postgres(stack, configuration)
    backup.restore(project, project.store(), bundle, digest)
    start_roles(stack)


def publish(stack, repo, generation, sequence, *, sync="docs-mcp-sync"):
    stack.exec("sync", sync, "--prepare", "--repository", repo, "--generation", generation)
    stack.exec("sync", sync, "--publish", "--repository", repo, "--generation", generation,
               "--expect-sequence", str(sequence))
    phase3.wait_until(
        lambda: phase2.sql(stack, f"""
            SELECT delivered_at IS NOT NULL FROM git_catalog.notification_outbox
            WHERE repository_id = '{repo}' AND publication_sequence = {sequence + 1}
        """) == "t", "publication notification did not arrive", seconds=40,
    )


def read(wire, repo, original):
    context = wire.resolve(repo)
    result = wire.call("git.read", {
        "context": context, "path": phase5.encoded("original.bin"),
        "range": {"start": 0, "end": 32}, "encoding": "base64",
    })
    assert b64decode(result["data"]) == original[:32]
    return context, result


def next_refs(wire, args, token):
    page = wire.call("git.refs", {**args, "page": {"limit": 1, "cursor": token}})
    return page["source"], page["entries"]


def rehearse(tag, source):
    def configure(stack, override):
        nonlocal current, files
        current, files = use_previous(stack, override, source, tag)

    current = files = None
    with phase2.stack(tag, control=True, configure_stack=configure) as (stack, upstream, approval_file, endpoint):
        previous = stack.compose_command, stack.context
        project_name = stack.compose_command[stack.compose_command.index("-p") + 1]
        project = backup.Project(project_name, files, stack.context)
        old = phase5.Wire(stack)
        old.validate_registry(legacy=True)
        records = {}
        for fmt, (repo, _) in phase2.REPOS.items():
            policy = phase2.approval(fmt)
            policy["exploration"] = {
                "version": 2, "metadata_notice": "Synthetic metadata",
                "scope": {"kind": "whole", "notice": "Synthetic bytes"},
                "outputs": {"search": True, "history": True},
            }
            phase2.register(stack, approval_file, policy)
            candidate = phase2.fetch(stack, repo, endpoint, binary="gh-docs-phase2-fixture")
            publish(stack, repo, candidate["generation"], 0, sync="gh-docs-sync")
            raw = (upstream / f"{fmt}-oracle.bin").read_bytes()
            records[repo] = raw, read(old, repo, raw)
        repo = phase2.REPOS["sha1"][0]
        generation = records[repo][1][0]["generation_id"]
        cursor_args = {"repository_id": repo, "generation_id": generation, "page": {"limit": 1}}
        old_cursor = old.call("git.refs", cursor_args)["next_cursor"]
        assert old_cursor
        expected_refs = next_refs(old, cursor_args, old_cursor)

        quiesce(stack)
        bundle = stack.root / "cutover-backup"
        captured = backup.backup(project, project.store(), bundle)
        catalog = backup.verify(bundle, captured["manifest_sha256"])["catalog"]
        canonical = backup.inventory(stack.root / "store")
        assert catalog["schema"] == 7
        start_postgres(stack, current)
        start_roles(stack)

        new = phase5.Wire(stack)
        new.validate_registry()
        assert json.loads(phase2.sql(stack, backup.CATALOG)) == {**catalog, "schema": 8}
        assert backup.inventory(stack.root / "store") == canonical
        for repository, (raw, expected) in records.items():
            assert read(new, repository, raw) == expected
        assert next_refs(new, cursor_args, old_cursor) == expected_refs
        new_cursor = new.call("git.refs", cursor_args)["next_cursor"]
        assert new_cursor

        git_repo = phase2.repo_path(upstream, "sha1")
        head, _, raw = phase2.commit(git_repo, "after rename", parent=records[repo][1][0]["commit_oid"])
        phase2.git(git_repo, "update-ref", "refs/heads/main", head)
        candidate = phase2.fetch(stack, repo, endpoint)
        assert candidate["evidence"]["baseline_generation"] == generation, candidate
        assert not candidate["evidence"]["baseline_corrupt"], candidate
        publish(stack, repo, candidate["generation"], 1)
        current_read = read(new, repo, raw)
        assert current_read[0]["commit_oid"] == head
        quiesce(stack)
        restore_previous(stack, previous, project, bundle, captured["manifest_sha256"])
        old.validate_registry(legacy=True)
        assert json.loads(phase2.sql(stack, backup.CATALOG)) == catalog
        for repository, (raw, expected) in records.items():
            assert read(old, repository, raw) == expected
        assert next_refs(old, cursor_args, new_cursor) == expected_refs
        print("Populated docs-mcp cutover to schema 8 and schema 7 backup rollback passed", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    parser.add_argument("--previous-source", type=Path, required=True,
                        help="disposable archive of the pre-rename checkout")
    args = parser.parse_args()
    source = args.previous_source.resolve()
    if not __debug__ or os.getuid() != 0:
        parser.error("run as root with Python assertions enabled")
    if not source.is_relative_to("/tmp") or not (source / "compose.yml").is_file():
        parser.error("use a pre-rename source archive under /tmp")
    rehearse(args.image_tag, source)
