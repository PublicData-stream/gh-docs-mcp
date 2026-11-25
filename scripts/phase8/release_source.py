#!/usr/bin/env python3
"""Verify the tagged release source and its canonical main-branch CI."""

# Adapted from rfc-mcp-docker. See docs/RFC-MCP-DOCKER-LICENSE.

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from release_version import check_workspace, parse_version, require, stage_source


REPOSITORY = "PublicData-stream/gh-docs-mcp"
CI_PATH = ".github/workflows/ci.yml"
CI_JOB = "rust"
SHA = re.compile(r"[0-9a-f]{40}")


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def api(endpoint, *, paginate=False):
    command = ["gh", "api"]
    if paginate:
        command.extend(("--paginate", "--slurp"))
    return json.loads(subprocess.check_output([*command, f"repos/{REPOSITORY}/{endpoint}"]))


def release_tag(version):
    match = parse_version(version)
    require(match["kind"] != "rc", "expected a stable X.Y.Z or beta X.Y.Z-beta.N release tag")
    return match["base"], match["kind"] or "stable"


def verify_event(event_name, event, version, kind):
    release = event.get("release", {})
    require(event_name == "release" and event.get("action") == "published"
            and release.get("tag_name") == version and release.get("draft") is False
            and release.get("prerelease") is (kind == "beta"),
            "expected the matching published GitHub Release")


def verify_tag(version, revision, tag_object):
    ref = f"refs/tags/{version}"
    current = api(f"git/ref/tags/{version}")
    require(current["ref"] == ref and current["object"]["type"] == "tag"
            and current["object"]["sha"] == tag_object, "remote annotated tag changed")
    tag = api(f"git/tags/{tag_object}")
    require(tag["sha"] == tag_object and tag["tag"] == version
            and tag["object"]["type"] == "commit" and tag["object"]["sha"] == revision
            and tag["verification"]["verified"] is True, "GitHub did not verify the signed release tag")


def canonical_run(run, revision):
    return (run["path"] == CI_PATH and run["event"] == "push"
            and run["head_branch"] == "main" and run["head_sha"] == revision
            and run["repository"]["full_name"] == REPOSITORY
            and (run.get("head_repository") or {}).get("full_name") == REPOSITORY)


def completed(check):
    return check["status"] == "completed" and check["conclusion"] == "success"


def verify_ci(revision):
    query = f"branch=main&event=push&head_sha={revision}&per_page=100"
    pages = api(f"actions/workflows/ci.yml/runs?{query}", paginate=True)
    runs = [run for page in pages for run in page["workflow_runs"] if canonical_run(run, revision)]
    require(runs, "no canonical main push CI run for the release commit")
    run = max(runs, key=lambda item: item["id"])
    attempt = run["run_attempt"]
    require(completed(run) and type(attempt) is int and attempt > 0,
            f"newest canonical CI did not pass: {run['html_url']}")
    pages = api(f"actions/runs/{run['id']}/attempts/{attempt}/jobs?per_page=100", paginate=True)
    checks = [job for page in pages for job in page["jobs"] if job["name"] == CI_JOB]
    require(len(checks) == 1, "expected one canonical Rust CI job")
    job, = checks
    check_id = re.fullmatch(rf"https://api\.github\.com/repos/{REPOSITORY}/check-runs/([0-9]+)",
                            job["check_run_url"], re.IGNORECASE)
    require(completed(job) and job["head_sha"] == revision and check_id,
            "Rust CI job did not pass for the release commit")
    check = api(f"check-runs/{check_id[1]}")
    require(check["name"] == CI_JOB and check["head_sha"] == revision and completed(check)
            and check["app"]["id"] == 15368 and check["app"]["slug"] == "github-actions"
            and check["details_url"] == job["html_url"], "unexpected Rust check identity")
    final = api(f"actions/runs/{run['id']}")
    require(canonical_run(final, revision) and completed(final) and final["run_attempt"] == attempt,
            "canonical CI changed attempt during release verification")
    print(f"Accepted Rust CI: {run['html_url']} (attempt {attempt})")


def verify_source(version, revision, tag_object):
    git("merge-base", "--is-ancestor", revision, "refs/remotes/origin/main")
    check_workspace(lambda name: git("show", f"{revision}:{name}"))
    verify_tag(version, revision, tag_object)
    verify_ci(revision)


def preflight(version):
    release_tag(version)
    ref = f"refs/tags/{version}"
    tag_object = git("rev-parse", "--verify", ref)
    revision = git("rev-parse", "--verify", f"{ref}^{{commit}}")
    verify_source(version, revision, tag_object)
    with tempfile.TemporaryDirectory(prefix="ghdocs8-preflight-", dir="/tmp") as directory:
        stage_source(revision, version, Path(directory))
    print(f"Release preflight passed for {version} at {revision}")


def main():
    args = sys.argv[1:]
    if len(args) == 2 and args[0] == "preflight":
        preflight(args[1])
        return
    require(not args or args == ["recheck"], "expected no arguments, recheck, or preflight X.Y.Z")
    require(os.environ["GITHUB_REPOSITORY"] == REPOSITORY, "unexpected release repository")
    ref = os.environ["GITHUB_REF"]
    require(ref.startswith("refs/tags/"), "release source must be a tag")
    version = ref.removeprefix("refs/tags/")
    _, kind = release_tag(version)
    revision = git("rev-parse", "HEAD")
    if args == ["recheck"]:
        require(version == os.environ["RELEASE_VERSION"] and revision == os.environ["RELEASE_REVISION"],
                "publication checkout differs from the verified source")
        tag_object = os.environ["RELEASE_TAG_OBJECT"]
        require(SHA.fullmatch(tag_object), "invalid release tag object")
        verify_tag(version, revision, tag_object)
        return
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_bytes())
    verify_event(os.environ["GITHUB_EVENT_NAME"], event, version, kind)
    tag_object = git("rev-parse", "--verify", ref)
    require(os.environ["GITHUB_SHA"] in {tag_object, revision}
            and git("rev-parse", f"{ref}^{{commit}}") == revision, "tag, event, and checkout differ")
    verify_source(version, revision, tag_object)
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        print(f"version={version}\nrevision={revision}\nkind={kind}\ntag_object={tag_object}", file=output)


if __name__ == "__main__":
    try:
        main()
    except (KeyError, OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
